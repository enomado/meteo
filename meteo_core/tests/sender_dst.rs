//! DST-lite отправителя: автомат гоняется против симулированной сети и
//! сенсора, которыми управляет лента байт от proptest (фиксированный seed ⇒
//! прогон детерминирован, падение воспроизводится и ужимается).
//!
//! Модель сети: `Write` принимает случайный префикс (частичная запись),
//! `Flush` отдаёт приёмнику всё неподтверждённое, обрыв может донести любой
//! префикс неподтверждённого и закрывает соединение (обрезанный кадр
//! выбрасывается). Бывают «аварии» — все сетевые операции падают подряд, пока
//! очередь не переполнится.
//!
//! Инварианты:
//! 1. приёмник никогда не видит битого кадра — ни фрейминга, ни расшифровки
//!    (ловит отправку пакета без дописывания хвоста);
//! 2. после стабилизации сети каждое показание либо доставлено, либо учтено как
//!    вытесненное из переполненной очереди, и не то и другое сразу (ловит
//!    чистку батча до подтверждения).

use std::collections::BTreeSet;
use std::sync::atomic::{
    AtomicU64,
    Ordering,
};

use aes_gcm::{
    Aes128Gcm,
    KeyInit,
};
use meteo_core::backlog::{
    SensorQueue,
    push_evicting,
};
use meteo_core::sender::{
    Action,
    Event,
    Sender,
};
use meteo_core::wire::{
    BaroReading,
    EpochMillis,
    LEN_PREFIX,
    PacketCounter,
    ScdReading,
    SensorData,
    decode_payload,
    payload_len,
};
use proptest::collection::vec;
use proptest::prelude::any;
use proptest::test_runner::{
    Config,
    RngSeed,
    TestCaseError,
    TestRunner,
};

fn cipher() -> Aes128Gcm {
    Aes128Gcm::new(&(*b"supersecretkey!1").into())
}

/// Байт ленты, переключающий «аварию сети».
const OUTAGE_TOGGLE: u8 = 0xFF;

/// Предел шагов стабилизации после конца ленты: 59 показаний в очереди +
/// батч в полёте дренятся за несколько десятков шагов.
const HEAL_STEPS: usize = 10_000;

/// Сторона приёмника одного TCP-соединения.
struct Receiver {
    cipher:  Aes128Gcm,
    stream:  Vec<u8>,
    counter: PacketCounter,
}

impl Receiver {
    fn new() -> Self {
        Self {
            cipher:  cipher(),
            stream:  Vec::new(),
            counter: PacketCounter::FIRST,
        }
    }

    /// Принять байты и разобрать все полные кадры.
    fn receive(&mut self, bytes: &[u8], delivered: &mut Vec<u64>) -> Result<(), String> {
        self.stream.extend_from_slice(bytes);
        while self.stream.len() >= LEN_PREFIX {
            let prefix = self.stream[..LEN_PREFIX].try_into().unwrap();
            let len = payload_len(prefix).map_err(|e| format!("framing broken: {e:?}"))?;
            if self.stream.len() < LEN_PREFIX + len {
                break;
            }
            let mut payload = self.stream[LEN_PREFIX..LEN_PREFIX + len].to_vec();
            let batch = decode_payload(&self.cipher, self.counter, &mut payload)
                .map_err(|e| format!("packet #{} rejected: {e:?}", self.counter.0))?;
            self.counter = self.counter.next();
            delivered.extend(batch.iter().map(|r| r.time.0));
            self.stream.drain(..LEN_PREFIX + len);
        }
        Ok(())
    }
}

/// Сколько раз сработал каждый интересный сценарий — чтобы зелёный прогон
/// доказывал, что сценарии вообще случались.
#[derive(Default)]
struct Coverage {
    partial_writes:      AtomicU64,
    write_failures:      AtomicU64,
    flush_failures:      AtomicU64,
    partial_deliveries:  AtomicU64,
    evictions:           AtomicU64,
    resent_after_errors: AtomicU64,
}

struct World {
    queue:     SensorQueue,
    next_id:   u64,
    produced:  BTreeSet<u64>,
    evicted:   BTreeSet<u64>,
    delivered: Vec<u64>,
    /// Байты, принятые сокетом в текущем соединении, но ещё не подтверждённые.
    unacked:   Vec<u8>,
    /// `Some` — соединение живо.
    receiver:  Option<Receiver>,
    outage:    bool,
}

impl World {
    fn new() -> Self {
        Self {
            queue:     SensorQueue::new(),
            next_id:   0,
            produced:  BTreeSet::new(),
            evicted:   BTreeSet::new(),
            delivered: Vec::new(),
            unacked:   Vec::new(),
            receiver:  None,
            outage:    false,
        }
    }

    /// Новое показание; размер строки зависит от id — пакеты разной длины.
    fn produce(&mut self, cov: &Coverage) {
        let id = self.next_id;
        self.next_id += 1;
        let reading = SensorData {
            baro: id.is_multiple_of(2).then_some(BaroReading {
                pressure: 101_325.0,
                temp:     21.5,
            }),
            scd:  (!id.is_multiple_of(3)).then_some(ScdReading {
                co2:      400 + (id % 2000) as u16,
                humidity: 45.0,
                temp:     22.0,
            }),
            time: EpochMillis(id),
        };
        self.produced.insert(id);
        if let Some(old) = push_evicting(&mut self.queue, reading) {
            self.evicted.insert(old.time.0);
            cov.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Обрыв соединения: до приёмника доходит префикс неподтверждённого.
    fn fail(&mut self, byte: u8, cov: &Coverage) -> Result<Event, String> {
        if let Some(mut rx) = self.receiver.take() {
            let k = (byte as usize * 7) % (self.unacked.len() + 1);
            if k > 0 {
                cov.partial_deliveries.fetch_add(1, Ordering::Relaxed);
            }
            rx.receive(&self.unacked[..k], &mut self.delivered)?;
        }
        self.unacked.clear();
        Ok(Event::Failed)
    }

    /// Исполнить действие. `byte` — выбор ленты; `None` — лента кончилась,
    /// сеть стабильна.
    fn perform(&mut self, action: Action<'_>, byte: Option<u8>, cov: &Coverage) -> Result<Event, String> {
        match action {
            Action::Sleep(_) => Ok(Event::Woke),
            Action::Connect => {
                let fails = match byte {
                    None => false,
                    Some(b) => self.outage || b % 5 == 0,
                };
                if fails {
                    return Ok(Event::Failed);
                }
                self.receiver = Some(Receiver::new());
                self.unacked.clear();
                Ok(Event::Connected)
            }
            Action::Write(bytes) => {
                let n = match byte {
                    None => bytes.len(),
                    Some(b) if self.outage || b % 7 == 0 => {
                        cov.write_failures.fetch_add(1, Ordering::Relaxed);
                        return self.fail(b, cov);
                    }
                    Some(b) if b % 2 == 0 => bytes.len(),
                    Some(b) => {
                        cov.partial_writes.fetch_add(1, Ordering::Relaxed);
                        1 + (b as usize * 13) % bytes.len()
                    }
                };
                self.unacked.extend_from_slice(&bytes[..n]);
                Ok(Event::Written(n))
            }
            Action::Flush => {
                if let Some(b) = byte
                    && (self.outage || b % 6 == 0)
                {
                    cov.flush_failures.fetch_add(1, Ordering::Relaxed);
                    return self.fail(b, cov);
                }
                let rx = self.receiver.as_mut().ok_or("Flush without a connection")?;
                rx.receive(&self.unacked, &mut self.delivered)?;
                self.unacked.clear();
                Ok(Event::Flushed)
            }
        }
    }
}

fn run(tape: &[u8], cov: &Coverage) -> Result<(), String> {
    let mut sender = Sender::new(cipher());
    let mut world = World::new();
    let mut event: Option<Event> = None;
    let mut saw_failure = false;

    for step in 0..tape.len() + HEAL_STEPS {
        let byte = tape.get(step).copied();
        match byte {
            Some(OUTAGE_TOGGLE) => world.outage = !world.outage,
            Some(b) if b % 3 == 0 => world.produce(cov),
            _ => {}
        }
        if byte.is_none() && world.queue.is_empty() && sender.in_flight() == 0 {
            // Стабилизация завершена: всё, что можно было доставить, доставлено.
            return check_conservation(&world);
        }

        // Батч пережил обрыв — следующий Write повторяет его.
        let retained = saw_failure && sender.in_flight() > 0;
        let action = match event {
            None => sender.start(),
            Some(ev) => sender.step(ev, &mut world.queue),
        };
        if retained && matches!(action, Action::Write(_)) {
            cov.resent_after_errors.fetch_add(1, Ordering::Relaxed);
            saw_failure = false;
        }
        let outcome = world.perform(action, byte, cov)?;
        saw_failure |= outcome == Event::Failed;
        event = Some(outcome);
    }
    Err(format!(
        "no progress: {} queued, {} in flight after {HEAL_STEPS} stable steps",
        world.queue.len(),
        sender.in_flight()
    ))
}

fn check_conservation(world: &World) -> Result<(), String> {
    let delivered: BTreeSet<u64> = world.delivered.iter().copied().collect();
    if let Some(id) = delivered.intersection(&world.evicted).next() {
        return Err(format!("reading {id} both delivered and evicted"));
    }
    let accounted: BTreeSet<u64> = delivered.union(&world.evicted).copied().collect();
    if let Some(id) = world.produced.difference(&accounted).next() {
        return Err(format!(
            "reading {id} lost: neither delivered nor evicted ({} produced, {} delivered, {} evicted)",
            world.produced.len(),
            delivered.len(),
            world.evicted.len()
        ));
    }
    Ok(())
}

#[test]
fn every_reading_is_delivered_or_evicted() {
    let mut runner = TestRunner::new(Config {
        cases: 512,
        rng_seed: RngSeed::Fixed(0x6d65_7465_6f5f_6473),
        failure_persistence: None,
        ..Config::default()
    });
    let cov = Coverage::default();
    runner
        .run(&vec(any::<u8>(), 0..4000), |tape| {
            run(&tape, &cov).map_err(TestCaseError::fail)
        })
        .unwrap();

    // Зелёный прогон что-то доказывает, только если сценарии случались.
    let seen = |c: &AtomicU64| c.load(Ordering::Relaxed);
    println!(
        "coverage: partial writes {}, write failures {}, flush failures {}, partial deliveries {}, \
         evictions {}, resends after errors {}",
        seen(&cov.partial_writes),
        seen(&cov.write_failures),
        seen(&cov.flush_failures),
        seen(&cov.partial_deliveries),
        seen(&cov.evictions),
        seen(&cov.resent_after_errors),
    );
    for (name, counter) in [
        ("partial writes", &cov.partial_writes),
        ("write failures", &cov.write_failures),
        ("flush failures", &cov.flush_failures),
        ("partial deliveries", &cov.partial_deliveries),
        ("evictions", &cov.evictions),
        ("resends after errors", &cov.resent_after_errors),
    ] {
        assert!(seen(counter) > 0, "scenario never happened: {name}");
    }
}
