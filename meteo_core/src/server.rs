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
use std::vec::Vec;

use aes_gcm::Aes128Gcm;

use crate::codec::{
    CodecError,
    Reading,
    Readings,
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
use crate::recv_window::{
    RecvWindow,
    SeqStatus,
};

/// Окно загрузки без датаграмм дольше этого — забывается.
pub const BOOT_IDLE_FORGET: Duration = Duration::from_secs(24 * 3600);

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
    /// Показания; ошибка разбора посреди — вся датаграмма отвергается до
    /// записи (см. [`Incoming::collect_readings`]).
    pub readings:  Readings<'a>,
}

impl Incoming<'_> {
    /// Все показания или первая ошибка разбора: частично записанная
    /// датаграмма была бы хуже отвергнутой целиком.
    pub fn collect_readings(self) -> Result<Vec<Reading>, ServerError> {
        self.readings
            .collect::<Result<_, _>>()
            .map_err(ServerError::Codec)
    }
}

struct BootState {
    /// `None` — от загрузки ещё ничего не записано.
    window:     Option<RecvWindow>,
    last_heard: Duration,
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
    pub fn new(cipher: Aes128Gcm, salt: ServerSalt) -> Self {
        Self {
            cipher,
            salt,
            next_ack: AckSeq::FIRST,
            boots: HashMap::new(),
        }
    }

    /// Проверить датаграмму (расшифровка на месте) и отдать её показания.
    pub fn receive<'a>(
        &mut self,
        datagram: &'a mut [u8],
        now: Duration,
    ) -> Result<Incoming<'a>, ServerError> {
        let (header, plain) = open_up(&self.cipher, datagram).map_err(ServerError::Datagram)?;
        let readings = decode(plain).map_err(ServerError::Codec)?;

        // Только подлинные датаграммы продлевают жизнь окнам.
        self.boots
            .retain(|_, b| now.saturating_sub(b.last_heard) < BOOT_IDLE_FORGET);
        let state = self.boots.entry(header.boot).or_insert(BootState {
            window:     None,
            last_heard: now,
        });
        state.last_heard = now;
        let duplicate = state
            .window
            .is_some_and(|w| w.status(header.seq) == SeqStatus::Seen);

        Ok(Incoming {
            boot: header.boot,
            seq: header.seq,
            ack_req: header.ack_req,
            duplicate,
            readings,
        })
    }

    /// Показания датаграммы записаны в базу. Звать только после успешной записи.
    pub fn ingested(&mut self, boot: BootId, seq: Seq, now: Duration) {
        let state = self.boots.entry(boot).or_insert(BootState {
            window:     None,
            last_heard: now,
        });
        state.last_heard = now;
        match &mut state.window {
            Some(w) => {
                // Ниже окна отметить нечем: прошивка сочтёт номер неизвестным
                // и переотправит показания под новым — запись идемпотентна.
                w.mark(seq);
            }
            None => state.window = Some(RecvWindow::new(seq)),
        }
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
    pub const fn new() -> Self {
        Self { until: None }
    }

    /// Запрос живых данных в момент `now`.
    pub fn request(&mut self, now: Duration, lease: Duration) {
        let until = now + lease;
        self.until = Some(self.until.map_or(until, |u| u.max(until)));
    }

    /// Остаток срока в целых секундах (вниз: прошивка не переживёт срок).
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
