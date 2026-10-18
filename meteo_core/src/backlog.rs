//! Передаточная очередь показаний sensor-таска → сетевая таска.
//!
//! Долгое хранение до подтверждения — бэклог отправителя
//! (`sender::BACKLOG`). Эта очередь развязывает таски, пока сетевая занята
//! отправкой.

use core::fmt::Debug;

use heapless::spsc::Queue;

use crate::codec::Reading;
use crate::sensors::Sample;
use crate::supervisor::Uptime;
use crate::wire::EpochMillis;

/// heapless `Queue<_, N>` вмещает N−1 элемент ⇒ 59 показаний ≈ 30 мин при
/// цикле ~30с; дальше вытесняются самые старые.
pub type SensorQueue = Queue<Reading, 60>;

/// Кладёт элемент. Очередь полна ⇒ вытесняет самый старый и возвращает его:
/// потеря учтена, а не молча проглочена.
///
/// # Panics
///
/// Паника возможна только при нарушении контракта `heapless::spsc::Queue`:
/// после неудачного `enqueue` очередь должна содержать элемент, а `dequeue`
/// обязана освободить место.
pub fn push_evicting<T: Debug, const N: usize>(queue: &mut Queue<T, N>, item: T) -> Option<T> {
    match queue.enqueue(item) {
        Ok(()) => None,
        Err(item) => {
            // Очередь полна ⇒ в ней есть хотя бы один элемент, и после dequeue
            // ровно одно место свободно: оба вызова не могут не сработать.
            let evicted = queue.dequeue().expect("full queue has at least one entry");
            queue.enqueue(item).expect("dequeue freed exactly one slot");
            Some(evicted)
        }
    }
}

/// Показания, снятые до первой синхронизации часов.
///
/// Момент цикла известен по uptime, а смещение uptime → epoch приходит с NTP
/// позже. Показание держим с его uptime и ставим ему epoch, когда смещение
/// появится, — иначе циклы до первого ответа NTP терялись. Ёмкость та же, что
/// у [`SensorQueue`]: 59 показаний ≈ 30 мин без NTP, дальше вытесняются самые
/// старые.
pub struct Unsynced(Queue<Held, 60>);

#[derive(Debug)]
struct Held {
    at:     Uptime,
    sample: Sample,
}

impl Unsynced {
    #[must_use]
    pub const fn new() -> Self {
        Self(Queue::new())
    }

    /// Часы ещё не синхронизированы: удержать показание цикла, завершившегося
    /// в `at`. Очередь полна ⇒ вытесняет самое старое и возвращает его момент.
    pub fn hold(&mut self, at: Uptime, sample: Sample) -> Option<Uptime> {
        push_evicting(&mut self.0, Held { at, sample }).map(|evicted| evicted.at)
    }

    /// Часы синхронизированы: следующее удержанное показание (старые первыми)
    /// со временем `epoch_at(момент цикла)`.
    pub fn release(&mut self, epoch_at: impl FnOnce(Uptime) -> EpochMillis) -> Option<Reading> {
        self.0
            .dequeue()
            .map(|held| held.sample.into_reading(epoch_at(held.at)))
    }
}

impl Default for Unsynced {
    fn default() -> Self {
        Self::new()
    }
}
