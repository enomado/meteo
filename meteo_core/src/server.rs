//! Приёмник протокола v2 как sans-IO автомат (фича `std`: окна по загрузкам
//! в `HashMap`). Сокет, база и часы — у вызывающего; здесь — порядок действий,
//! от которого зависит доставка:
//!
//! 1. [`Server::receive`] проверяет датаграмму и говорит, дубль ли это;
//! 2. не дубль ⇒ вызывающий пишет показания в базу и ТОЛЬКО при успехе зовёт
//!    [`Server::ingested`] — отметка до записи потеряла бы показания при сбое
//!    базы (прошивка удалила бы их по подтверждению);
//! 3. `ack_req` и (дубль или запись удалась) ⇒ [`Server::ack`]. Сбой записи ⇒
//!    ни отметки, ни подтверждения: прошивка переотправит.
//!
//! Время — `Duration` от старта процесса приёмника (монотонное).

use core::time::Duration;
use std::collections::HashMap;
use std::string::String;
use std::vec::Vec;

use aes_gcm::Aes128Gcm;

use crate::codec::{
    CodecError,
    HealthRecord,
    Reading,
    Record,
    Records,
    decode,
};
use crate::datagram::{
    ACK_DATAGRAM_LEN,
    Ack,
    AckSeq,
    BootId,
    DatagramError,
    DownHeader,
    LiveSecs,
    Seq,
    ServerSalt,
    open_up,
};
use crate::health::{
    Kind,
    NetStage,
    STALL_EXECUTOR,
    STALL_NET,
    STALL_SENSOR,
    Snapshot,
    Word,
};
use crate::recv_window::{
    RecvWindow,
    SeqStatus,
};
use crate::sensors::{
    Fault,
    OpCode,
};

/// Окно загрузки без датаграмм дольше этого — забывается.
pub const BOOT_IDLE_FORGET: Duration = Duration::from_hours(24);

/// Почему датаграмма не принята.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerError {
    Datagram(DatagramError),
    /// Тег сошёлся, а показания не разбираются — дефект отправителя.
    Codec(CodecError),
}

/// Проверенная датаграмма прошивки.
pub struct Incoming<'a> {
    pub boot:      BootId,
    pub seq:       Seq,
    pub ack_req:   bool,
    /// Уже записана в базу: писать повторно не нужно, подтвердить — да.
    pub duplicate: bool,
    /// Записи; ошибка разбора посреди — вся датаграмма отвергается до
    /// записи (см. [`Incoming::collect`]).
    pub records:   Records<'a>,
}

/// Разобранное содержимое датаграммы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contents {
    pub readings: Vec<Reading>,
    /// Снимок журнала здоровья, если прошивка его приложила.
    pub health:   Option<HealthRecord>,
}

impl Incoming<'_> {
    /// Всё содержимое или первая ошибка разбора: частично записанная
    /// датаграмма была бы хуже отвергнутой целиком.
    ///
    /// # Errors
    ///
    /// Возвращает [`ServerError::Codec`], если хотя бы одна запись не
    /// разбирается.
    pub fn collect(self) -> Result<Contents, ServerError> {
        let mut contents = Contents {
            readings: Vec::new(),
            health:   None,
        };
        for record in self.records {
            match record.map_err(ServerError::Codec)? {
                Record::Reading(r) => contents.readings.push(r),
                Record::Health(h) => contents.health = Some(h),
            }
        }
        Ok(contents)
    }
}

/// Последний принятый снимок здоровья платы и что в нём изменилось.
///
/// Порядок: внутри одной загрузки (`BootId`) поколение растёт ⇒ снимок не
/// новее последнего — опоздавший дубль или перестановка, пропускаем. Новая
/// загрузка принимается всегда (журнал переживает reset и продолжается).
/// Подача питания обнуляет журнал: счётчик загрузок упал ⇒ сравнивать не с
/// чем, изменения — все ненулевые поля.
#[derive(Debug, Default)]
pub struct HealthLog {
    last: Option<(BootId, Snapshot)>,
}

/// Изменение одного поля снимка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthChange {
    pub word: Word,
    pub was:  u32,
    pub now:  u32,
}

impl HealthLog {
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Принять снимок загрузки `boot`. `None` — снимок не новее последнего.
    pub fn accept(&mut self, boot: BootId, snapshot: Snapshot) -> Option<Vec<HealthChange>> {
        let prev = match &self.last {
            Some((last_boot, last)) if *last_boot == boot && snapshot.generation() <= last.generation() => {
                return None;
            }
            Some((_, last)) if snapshot.get(Word::BootTotal) < last.get(Word::BootTotal) => None,
            Some((_, last)) => Some(*last),
            None => None,
        };
        let changes = snapshot
            .changes(prev.as_ref())
            .map(|(word, was, now)| HealthChange { word, was, now })
            .collect();
        self.last = Some((boot, snapshot));
        Some(changes)
    }
}

impl core::fmt::Display for HealthChange {
    /// `scd41.timeout +1 (=3)` для счётчиков; значения — с расшифровкой кода.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = self.word.name();
        if self.word.kind() == Kind::Counter {
            return write!(f, "{name} +{} (={})", self.now.wrapping_sub(self.was), self.now);
        }
        write!(f, "{name} = {}", self.now)?;
        decode_value(self.word, self.now).map_or(Ok(()), |meaning| write!(f, " ({meaning})"))
    }
}

/// Расшифровка кодов в значениях журнала.
fn decode_value(word: Word, value: u32) -> Option<String> {
    match word {
        Word::Scd41LastOp | Word::Bmp390LastOp | Word::StallSensorStage => Some(OpCode(value).name().into()),
        Word::Scd41LastFault | Word::Bmp390LastFault => Some(Fault::name_of(value).into()),
        Word::StallNetStage => Some(NetStage::name_of(value).into()),
        Word::StallTasks => {
            let names: Vec<&str> = [
                (STALL_SENSOR, "sensor"),
                (STALL_NET, "net"),
                (STALL_EXECUTOR, "executor"),
            ]
            .into_iter()
            .filter(|(bit, _)| value & bit != 0)
            .map(|(_, name)| name)
            .collect();
            Some(if names.is_empty() {
                "none".into()
            } else {
                names.join("+")
            })
        }
        _ => None,
    }
}

struct BootState {
    /// `None` — от загрузки ещё ничего не записано.
    window:     Option<RecvWindow>,
    last_heard: Duration,
}

impl BootState {
    fn ingested(&mut self, seq: Seq, now: Duration) {
        self.last_heard = now;
        match &mut self.window {
            Some(w) => {
                // Ниже окна отметить нечем: прошивка сочтёт номер неизвестным
                // и переотправит показания под новым — запись идемпотентна.
                w.mark(seq);
            }
            None => self.window = Some(RecvWindow::new(seq)),
        }
    }
}

pub struct Server {
    cipher:   Aes128Gcm,
    salt:     ServerSalt,
    next_ack: AckSeq,
    boots:    HashMap<BootId, BootState>,
}

impl Server {
    /// `salt` — случайный u32 на каждый старт процесса (половина nonce
    /// подтверждений; повтор соли = повтор nonce).
    #[must_use]
    pub fn new(cipher: Aes128Gcm, salt: ServerSalt) -> Self {
        Self {
            cipher,
            salt,
            next_ack: AckSeq::FIRST,
            boots: HashMap::new(),
        }
    }

    /// Проверить датаграмму (расшифровка на месте) и отдать её показания.
    ///
    /// # Errors
    ///
    /// Возвращает [`ServerError::Datagram`], если датаграмма не проходит
    /// проверку подлинности или структуры, и [`ServerError::Codec`], если
    /// её показания не разбираются.
    pub fn receive<'a>(
        &mut self,
        datagram: &'a mut [u8],
        now: Duration,
    ) -> Result<Incoming<'a>, ServerError> {
        let uplink = open_up(&self.cipher, datagram).map_err(ServerError::Datagram)?;
        let records = decode(uplink.plaintext).map_err(ServerError::Codec)?;

        // Только подлинные датаграммы продлевают жизнь окнам.
        self.boots
            .retain(|_, b| now.saturating_sub(b.last_heard) < BOOT_IDLE_FORGET);
        let state = self.boots.entry(uplink.header.boot).or_insert(BootState {
            window:     None,
            last_heard: now,
        });
        state.last_heard = now;
        let duplicate = state
            .window
            .is_some_and(|w| w.status(uplink.header.seq) == SeqStatus::Seen);

        Ok(Incoming {
            boot: uplink.header.boot,
            seq: uplink.header.seq,
            ack_req: uplink.header.ack_req,
            duplicate,
            records,
        })
    }

    /// Показания датаграммы записаны в базу. Звать только после успешной записи.
    pub fn ingested(&mut self, boot: BootId, seq: Seq, now: Duration) {
        let state = self.boots.entry(boot).or_insert(BootState {
            window:     None,
            last_heard: now,
        });
        state.ingested(seq, now);
    }

    /// Подтверждение для загрузки `boot`. `None` — от неё ещё ничего не
    /// записано: подтверждать нечего.
    pub fn ack(&mut self, boot: BootId, live_for: LiveSecs) -> Option<[u8; ACK_DATAGRAM_LEN]> {
        let window = self.boots.get(&boot)?.window?;
        let header = DownHeader {
            salt:    self.salt,
            ack_seq: self.next_ack,
        };
        self.next_ack = self.next_ack.next();
        Some(
            Ack {
                boot,
                window,
                live_for,
            }
            .seal(&self.cipher, header),
        )
    }
}

/// Срок живого режима, который держит приёмник: каждый запрос живых данных
/// (открытая страница графика) продлевает его на `lease`. Остаток уходит
/// прошивке в подтверждении как `live_for`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveLease {
    /// `None` — живые данные никто не запрашивал.
    until: Option<Duration>,
}

impl LiveLease {
    #[must_use]
    pub const fn new() -> Self {
        Self { until: None }
    }

    /// Запрос живых данных в момент `now`.
    pub fn request(&mut self, now: Duration, lease: Duration) {
        let until = now + lease;
        self.until = Some(self.until.map_or(until, |u| u.max(until)));
    }

    /// Остаток срока в целых секундах (вниз: прошивка не переживёт срок).
    #[must_use]
    pub fn remaining(&self, now: Duration) -> LiveSecs {
        let left = self.until.map_or(Duration::ZERO, |u| u.saturating_sub(now));
        LiveSecs(u16::try_from(left.as_secs()).unwrap_or(u16::MAX))
    }
}

impl Default for LiveLease {
    fn default() -> Self {
        Self::new()
    }
}
