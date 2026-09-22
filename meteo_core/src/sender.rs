//! Отправитель протокола v2 (UDP) как sans-IO автомат с явным временем.
//!
//! Драйвер прошивки кормит его показаниями ([`Sender::on_reading`]) и
//! датаграммами сервера ([`Sender::on_datagram`]), отправляет то, что отдаёт
//! [`Sender::poll`], сообщает о сбое отправки ([`Sender::on_send_failed`]) и
//! спит до [`Sender::deadline`]. Время — `Uptime` в каждом вызове; сокета и
//! часов автомат не трогает ⇒ DST гоняет его на виртуальном времени.
//!
//! Доставка: показание живёт в бэклоге, пока подтверждение сервера не скажет,
//! что его датаграмма записана в базу (сервер отмечает окно ПОСЛЕ записи).
//! Датаграмма без ответа, ниже окна или пропущенная окном ⇒ показание снова
//! `Pending` и уйдёт в НОВОЙ датаграмме с новым `Seq` (новый nonce; дубль на
//! сервере безвреден — запись идемпотентна по времени показания).
//!
//! Режимы:
//! - `Batch` — раз в `BATCH_INTERVAL` все `Pending` (старые первыми, сколько
//!   влезет) одной датаграммой с `ACK_REQ`; не влезло ⇒ следующая сразу после
//!   подтверждения. Одна датаграмма в полёте.
//! - `Live` — `Pending` уходит сразу; `ACK_REQ` раз в `ACK_EVERY_LIVE`
//!   датаграмм или когда до конца срока ≤ `LIVE_RENEW_MARGIN` (подтверждение
//!   несёт продление). Срок задаёт сервер в подтверждении.
//!
//! Ожидание подтверждения одно на запрос; таймаут или сбой отправки ⇒ всё в
//! полёте снова `Pending`, пауза `RETRY` с удвоением до `BATCH_INTERVAL`.

use core::time::Duration;

use aes_gcm::Aes128Gcm;

use crate::codec::{
    Encoder,
    Reading,
};
use crate::datagram::{
    BootId,
    DatagramBuf,
    DatagramError,
    Header,
    MAX_DATAGRAM,
    Seq,
    UpHeader,
    open_ack,
    plain_area,
    seal,
};
use crate::recv_window::SeqStatus;
use crate::supervisor::Uptime;

/// Период батчей: компромисс трафика и задержки включения живого режима
/// (сигнал едет в подтверждении батча).
pub const BATCH_INTERVAL: Duration = Duration::from_secs(120);
/// Сколько ждать подтверждения на запрос.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// Первая пауза после сбоя; дальше удваивается до `BATCH_INTERVAL`.
pub const RETRY: Duration = Duration::from_secs(15);
/// Потолок срока живого режима, что бы ни прислал сервер.
pub const MAX_LIVE: Duration = Duration::from_secs(600);
/// В живом режиме подтверждение запрашивается не реже раза на столько датаграмм.
pub const ACK_EVERY_LIVE: u32 = 8;
/// До конца срока осталось столько ⇒ запросить подтверждение (оно продлит срок).
pub const LIVE_RENEW_MARGIN: Duration = Duration::from_secs(90);
/// Ёмкость бэклога: 12 ч при цикле замеров 30 с.
pub const BACKLOG: usize = 1440;

/// Где показание в пути.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    /// Ждёт отправки.
    Pending,
    /// Ушло в датаграмме с этим номером, судьба неизвестна.
    InFlight(Seq),
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    reading: Reading,
    state:   SlotState,
}

/// Хранилище показаний отправителя (~80 КБ при `BACKLOG` = 1440). Отдельный
/// тип с `const fn new`: прошивка держит его в `static`, собранном при
/// компиляции, — значение такого размера, созданное в рантайме, прошло бы
/// через стек MCU.
pub struct Backlog(heapless::Vec<Slot, BACKLOG>);

impl Backlog {
    pub const fn new() -> Self {
        Self(heapless::Vec::new())
    }
}

impl Default for Backlog {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Batch,
    /// Живой режим до `until`.
    Live {
        until: Uptime,
    },
}

/// Отправленный запрос подтверждения.
#[derive(Debug, Clone, Copy)]
struct AckWait {
    seq:      Seq,
    deadline: Uptime,
}

/// Почему датаграмма сервера не принята.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckError {
    Datagram(DatagramError),
    /// Подтверждение адресовано другой загрузке (прошлой жизни прошивки).
    ForeignBoot,
}

fn after(t: Uptime, d: Duration) -> Uptime {
    Uptime(t.0 + d)
}

pub struct Sender<'b> {
    cipher:        Aes128Gcm,
    boot:          BootId,
    next_seq:      Seq,
    backlog:       &'b mut heapless::Vec<Slot, BACKLOG>,
    mode:          Mode,
    /// Когда в `Batch` может уйти следующая датаграмма.
    next_batch:    Uptime,
    awaiting:      Option<AckWait>,
    /// После сбоя: ничего не отправляем до этого момента.
    hold_until:    Option<Uptime>,
    /// Пауза для следующего сбоя.
    backoff:       Duration,
    /// Датаграмм в живом режиме с последнего `ACK_REQ`.
    since_ack_req: u32,
    /// Последний запрос подтверждения получил ответ (для LED «нет сервера»).
    reachable:     bool,
    buf:           DatagramBuf,
}

impl<'b> Sender<'b> {
    /// Показания, уже лежащие в `backlog`, считаются неотправленными.
    pub fn new(cipher: Aes128Gcm, boot: BootId, now: Uptime, backlog: &'b mut Backlog) -> Self {
        for slot in backlog.0.iter_mut() {
            slot.state = SlotState::Pending;
        }
        Self {
            cipher,
            boot,
            next_seq: Seq::FIRST,
            backlog: &mut backlog.0,
            mode: Mode::Batch,
            next_batch: after(now, BATCH_INTERVAL),
            awaiting: None,
            hold_until: None,
            backoff: RETRY,
            since_ack_req: 0,
            reachable: true,
            buf: [0u8; MAX_DATAGRAM],
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Показаний в бэклоге (ждут отправки или подтверждения).
    pub fn backlog_len(&self) -> usize {
        self.backlog.len()
    }

    /// `false` — последний запрос подтверждения остался без ответа.
    pub fn server_reachable(&self) -> bool {
        self.reachable
    }

    /// Новое показание. Бэклог полон ⇒ вытесняется самое старое и
    /// возвращается: потеря учтена, а не проглочена.
    pub fn on_reading(&mut self, reading: Reading) -> Option<Reading> {
        let evicted = self.backlog.is_full().then(|| self.backlog.remove(0).reading);
        self.backlog
            .push(Slot {
                reading,
                state: SlotState::Pending,
            })
            .expect("a slot was freed above if the backlog was full");
        evicted
    }

    /// Датаграмма с адреса сервера (расшифровка на месте).
    pub fn on_datagram(&mut self, datagram: &mut [u8], now: Uptime) -> Result<(), AckError> {
        let (_, ack) = open_ack(&self.cipher, datagram).map_err(AckError::Datagram)?;
        if ack.boot != self.boot {
            return Err(AckError::ForeignBoot);
        }

        // Окно говорит правду о любой датаграмме, даже из опоздавшего
        // подтверждения: бит ставится только после записи в базу.
        self.backlog.retain_mut(|slot| {
            let SlotState::InFlight(seq) = slot.state else {
                return true;
            };
            match ack.window.status(seq) {
                SeqStatus::Seen => false,
                SeqStatus::NotSeen | SeqStatus::OutOfWindow => {
                    slot.state = SlotState::Pending;
                    true
                }
                SeqStatus::Ahead => true,
            }
        });

        // Ответ на текущий запрос: сервер жив, режим — по его слову. Опоздавшие
        // подтверждения режим не трогают — они старше последнего запроса.
        if let Some(wait) = self.awaiting
            && ack.window.highest() >= wait.seq
        {
            self.awaiting = None;
            self.hold_until = None;
            self.backoff = RETRY;
            self.reachable = true;
            let live_for = Duration::from_secs(u64::from(ack.live_for.0)).min(MAX_LIVE);
            self.mode = if live_for.is_zero() {
                if matches!(self.mode, Mode::Live { .. }) {
                    self.next_batch = after(now, BATCH_INTERVAL);
                }
                Mode::Batch
            } else {
                Mode::Live {
                    until: after(now, live_for),
                }
            };
        }
        Ok(())
    }

    /// Датаграмма из последнего `poll` не ушла (ошибка сокета).
    pub fn on_send_failed(&mut self, now: Uptime) {
        self.fail(now);
    }

    /// Когда разбудить автомат (таймаут подтверждения, конец паузы, конец
    /// живого режима, очередной батч). `None` — ждать нечего, только показаний
    /// и датаграмм.
    pub fn deadline(&self) -> Option<Uptime> {
        let batch = (self.mode == Mode::Batch
            && self.awaiting.is_none()
            && self.hold_until.is_none()
            && self.has_pending())
        .then_some(self.next_batch);
        let live_end = match self.mode {
            Mode::Live { until } => Some(until),
            Mode::Batch => None,
        };
        [
            self.awaiting.map(|w| w.deadline),
            self.hold_until,
            live_end,
            batch,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Следующая датаграмма к отправке или `None`. Звать до `None` после
    /// каждого события: в живом режиме бэклог может уйти несколькими.
    pub fn poll(&mut self, now: Uptime) -> Option<&[u8]> {
        self.expire(now);
        if self.hold_until.is_some() || !self.has_pending() {
            return None;
        }
        let ack_req = match self.mode {
            Mode::Batch => {
                if self.awaiting.is_some() || now < self.next_batch {
                    return None;
                }
                true
            }
            Mode::Live { until } => {
                self.awaiting.is_none()
                    && (self.since_ack_req + 1 >= ACK_EVERY_LIVE
                        || until.0.saturating_sub(now.0) <= LIVE_RENEW_MARGIN)
            }
        };
        Some(self.compose(now, ack_req))
    }

    fn has_pending(&self) -> bool {
        self.backlog.iter().any(|s| s.state == SlotState::Pending)
    }

    /// Истечения по времени: таймаут подтверждения, конец паузы, конец срока.
    fn expire(&mut self, now: Uptime) {
        if let Some(wait) = self.awaiting
            && now >= wait.deadline
        {
            self.fail(now);
        }
        if let Some(until) = self.hold_until
            && now >= until
        {
            // Пауза кончилась: пробная датаграмма уходит сразу, с запросом.
            self.hold_until = None;
            self.next_batch = now;
        }
        if let Mode::Live { until } = self.mode
            && now >= until
        {
            self.mode = Mode::Batch;
            self.next_batch = now;
        }
        // В `Batch` каждая датаграмма просит подтверждения ⇒ без ожидания в
        // полёте может остаться только хвост живого режима, чей запрос так и
        // не ушёл. Никто его не подтвердит — отправляем заново.
        if self.mode == Mode::Batch && self.awaiting.is_none() {
            self.requeue_in_flight();
        }
    }

    fn fail(&mut self, now: Uptime) {
        self.requeue_in_flight();
        self.awaiting = None;
        self.reachable = false;
        self.hold_until = Some(after(now, self.backoff));
        self.backoff = (self.backoff * 2).min(BATCH_INTERVAL);
        // Первая датаграмма после паузы просит подтверждения и в живом режиме.
        self.since_ack_req = ACK_EVERY_LIVE;
    }

    fn requeue_in_flight(&mut self) {
        for slot in self.backlog.iter_mut() {
            slot.state = SlotState::Pending;
        }
    }

    /// Собрать датаграмму из `Pending` (старые первыми, сколько влезет).
    fn compose(&mut self, now: Uptime, ack_req: bool) -> &[u8] {
        let seq = self.next_seq;
        self.next_seq = seq.next();

        let first = self
            .backlog
            .iter()
            .find(|s| s.state == SlotState::Pending)
            .expect("compose runs only with pending readings")
            .reading
            .time;
        let mut encoder = Encoder::new(plain_area(&mut self.buf), first);
        let mut full = false;
        for slot in self.backlog.iter_mut().filter(|s| s.state == SlotState::Pending) {
            if !encoder.push(&slot.reading) {
                full = true;
                break;
            }
            slot.state = SlotState::InFlight(seq);
        }
        let plain_len = encoder.plain_len();

        if ack_req {
            self.awaiting = Some(AckWait {
                seq,
                deadline: after(now, ACK_TIMEOUT),
            });
            self.since_ack_req = 0;
        } else {
            self.since_ack_req += 1;
        }
        if self.mode == Mode::Batch {
            // Не влезло всё ⇒ остаток уходит сразу после подтверждения.
            self.next_batch = if full { now } else { after(now, BATCH_INTERVAL) };
        }

        let header = Header::Up(UpHeader {
            boot: self.boot,
            seq,
            ack_req,
        });
        let len = seal(&self.cipher, header, &mut self.buf, plain_len);
        &self.buf[..len]
    }
}
