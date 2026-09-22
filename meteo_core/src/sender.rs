//! Отправитель пакетов как sans-IO конечный автомат.
//!
//! Автомат не трогает сокет и время: принимает `Event` (итог предыдущего
//! действия) и выдаёт следующее `Action`. Таска-драйвер прошивки исполняет
//! действие на настоящем сокете и возвращает итог. Трейт-абстракции над
//! сокетом нет — драйвер один, тест (DST) исполняет действия сам.
//!
//! Доставка: батч чистится только после `Flushed` — удалённый TCP подтвердил
//! все байты. Ошибка до этого ⇒ тот же батч уйдёт на новом соединении целиком
//! (приёмник идемпотентен по времени показания). Пакет пишется, пока сокет не
//! примет его весь: частичная запись без дописывания рвала фрейминг потока.

use core::time::Duration;

use aes_gcm::Aes128Gcm;

use crate::backlog::SensorQueue;
use crate::wire::{
    BUF_LEN,
    PacketBuf,
    PacketCounter,
    SensorBatch,
    encode_packet,
};

/// Пауза перед повтором connect после неудачи.
pub const CONNECT_RETRY: Duration = Duration::from_secs(5);
/// Пауза перед реконнектом после обрыва отправки.
pub const SEND_RETRY: Duration = Duration::from_secs(3);
/// Пауза между доставленными пакетами: backlog дренится без залпа.
pub const SEND_PACING: Duration = Duration::from_secs(3);
/// Опрос пустой очереди.
pub const EMPTY_POLL: Duration = Duration::from_secs(1);

/// Итог действия, исполненного драйвером.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// `Connect` удался.
    Connected,
    /// Сокет принял первые `n` байт из `Write` (1 ≤ n ≤ длины среза).
    Written(usize),
    /// `Flush`: удалённый TCP подтвердил всё записанное, соединение живо.
    Flushed,
    /// Любая ошибка или таймаут сокета — соединение считается мёртвым, драйвер
    /// его уже закрыл.
    Failed,
    /// `Sleep` закончился.
    Woke,
}

/// Что драйверу сделать дальше.
#[derive(PartialEq, Eq, Debug)]
pub enum Action<'a> {
    /// Установить новое TCP-соединение.
    Connect,
    /// Записать в сокет эти байты (сколько примет — вернуть в `Written`).
    Write(&'a [u8]),
    /// Дождаться ACK на всё записанное.
    Flush,
    Sleep(Duration),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Ждём итога `Connect`.
    Connecting,
    /// Спим перед `Connect`.
    Backoff,
    /// Соединение живо, спим перед следующим пакетом.
    Idle,
    /// Пишем пакет `packet[..len]`, первые `sent` байт уже в сокете.
    Writing { sent: usize, len: usize },
    /// Пакет записан целиком, ждём ACK.
    Flushing,
}

pub struct Sender {
    cipher:  Aes128Gcm,
    phase:   Phase,
    /// Показания в полёте: взяты из очереди, но ещё не подтверждены.
    batch:   SensorBatch,
    /// Номер следующего пакета в текущем соединении.
    counter: PacketCounter,
    packet:  PacketBuf,
}

impl Sender {
    pub fn new(cipher: Aes128Gcm) -> Self {
        Self {
            cipher,
            phase: Phase::Connecting,
            batch: SensorBatch::new(),
            counter: PacketCounter::FIRST,
            packet: [0u8; BUF_LEN],
        }
    }

    /// Первое действие после создания.
    pub fn start(&self) -> Action<'_> {
        debug_assert_eq!(self.phase, Phase::Connecting);
        Action::Connect
    }

    /// Сколько показаний сейчас в полёте (для лога).
    pub fn in_flight(&self) -> usize {
        self.batch.len()
    }

    /// Обработать итог предыдущего действия. `queue` — источник показаний;
    /// трогается, только когда батч пуст и соединение готово к пакету.
    ///
    /// Паника — нарушение контракта драйвером: событие не является итогом
    /// выданного действия (например, `Flushed` в ответ на `Sleep`).
    pub fn step(&mut self, event: Event, queue: &mut SensorQueue) -> Action<'_> {
        match (self.phase, event) {
            (Phase::Connecting, Event::Connected) => {
                // Приёмник считает пакеты с FIRST на каждом соединении.
                self.counter = PacketCounter::FIRST;
                self.next_packet(queue)
            }
            (Phase::Connecting, Event::Failed) => self.backoff(CONNECT_RETRY),
            (Phase::Backoff, Event::Woke) => {
                self.phase = Phase::Connecting;
                Action::Connect
            }
            (Phase::Idle, Event::Woke) => self.next_packet(queue),
            (Phase::Writing { sent, len }, Event::Written(n)) => {
                let remaining = len - sent;
                assert!(
                    (1..=remaining).contains(&n),
                    "sender: Written({n}) for {remaining} pending bytes"
                );
                let sent = sent + n;
                if sent == len {
                    self.phase = Phase::Flushing;
                    Action::Flush
                } else {
                    self.phase = Phase::Writing { sent, len };
                    Action::Write(&self.packet[sent..len])
                }
            }
            (Phase::Flushing, Event::Flushed) => {
                self.batch.clear();
                self.phase = Phase::Idle;
                Action::Sleep(SEND_PACING)
            }
            // Батч НЕ чистим: без ACK неизвестно, дошёл ли он.
            (Phase::Writing { .. } | Phase::Flushing, Event::Failed) => self.backoff(SEND_RETRY),
            (phase, event) => panic!("sender: {event:?} is not an outcome of the action issued in {phase:?}"),
        }
    }

    fn backoff(&mut self, pause: Duration) -> Action<'_> {
        self.phase = Phase::Backoff;
        Action::Sleep(pause)
    }

    /// Соединение живо и свободно: собрать батч (или продолжить недоставленный)
    /// и начать запись пакета.
    fn next_packet(&mut self, queue: &mut SensorQueue) -> Action<'_> {
        if self.batch.is_empty() {
            // Не больше MAX_BATCH за пакет (ёмкость батча): backlog дренится
            // несколькими пакетами.
            while !self.batch.is_full() {
                let Some(reading) = queue.dequeue() else {
                    break;
                };
                self.batch
                    .push(reading)
                    .expect("loop runs only while the batch has room");
            }
        }
        if self.batch.is_empty() {
            self.phase = Phase::Idle;
            return Action::Sleep(EMPTY_POLL);
        }

        let len = encode_packet(&self.cipher, self.counter, &self.batch, &mut self.packet);
        self.counter = self.counter.next();
        self.phase = Phase::Writing { sent: 0, len };
        Action::Write(&self.packet[..len])
    }
}
