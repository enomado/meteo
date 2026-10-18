//! Приёмник протокола v2 под произвольной историей датаграмм, сверка с
//! независимой моделью.
//!
//! Вход — последовательность событий: подлинная датаграмма (открытый текст —
//! произвольные байты или выход настоящего кодировщика), её повтор, подделка
//! (мусор, чужой ключ, обратное направление, порча подлинной), течение времени.
//! Цикл — как у `example_server`: `receive` → `collect` → журнал здоровья →
//! `ingested` → `ack`. Инварианты:
//!
//! 1. ничего не паникует, включая `Display` изменений журнала;
//! 2. подделка отвергается как `ServerError::Datagram` и не меняет состояния
//!    (в том числе не продлевает жизнь окну загрузки);
//! 3. подлинная датаграмма отвергается ⇔ не разбирается её `base_time`;
//! 4. `duplicate` ⇔ номер записан и ещё в окне: ложный дубль = потеря
//!    показаний (их не запишут, а прошивка удалит по подтверждению);
//! 5. подтверждение есть ⇔ от загрузки что-то записано; его окно совпадает с
//!    моделью, номера подтверждений растут на единицу;
//! 6. принятые показания переживают перекодирование настоящим кодировщиком;
//! 7. журнал здоровья пропускает снимок ⇔ та же загрузка и поколение не
//!    выросло; каждое изменение называет нынешнее значение поля.

#![no_main]

use std::collections::{
    BTreeSet,
    HashMap,
};
use std::sync::Once;
use std::sync::atomic::{
    AtomicU64,
    Ordering,
};
use std::time::Duration;

use aes_gcm::{
    Aes128Gcm,
    KeyInit,
};
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use meteo_core::codec::{
    CentiPascal,
    Encoder,
    MilliCelsius,
    MilliPercentRh,
    Ppm,
    Reading,
    Record,
    decode,
};
use meteo_core::datagram::{
    Ack,
    AckSeq,
    BootId,
    DownHeader,
    Header,
    LiveSecs,
    MAX_DATAGRAM,
    PLAIN_CAPACITY,
    Seq,
    ServerSalt,
    UpHeader,
    open_ack,
    plain_area,
    seal,
};
use meteo_core::health::{
    SNAPSHOT_WORDS,
    Snapshot,
};
use meteo_core::recv_window::{
    RecvWindow,
    WINDOW_BITS,
};
use meteo_core::server::{
    BOOT_IDLE_FORGET,
    HealthLog,
    Server,
    ServerError,
};
use meteo_core::wire::EpochMillis;

/// Сколько раз сработала каждая проверяемая ветка: «расхождений нет» без
/// счётчиков ничего не доказывает. Печать при выходе — с `METEO_FUZZ_STATS=1`
/// (прогон корпуса: `cargo fuzz run receiver corpus/receiver -- -runs=0`).
#[derive(Clone, Copy)]
enum Stat {
    Accepted,
    BaseRejected,
    CollectFailed,
    Duplicate,
    Stored,
    Acked,
    NothingToAck,
    Forgotten,
    HealthAccepted,
    HealthStale,
    HealthChange,
    Readings,
    ForgedGarbage,
    ForgedForeignKey,
    ForgedDownlink,
    ForgedFlip,
    ForgedTruncate,
    ForgedExtend,
}

const STAT_NAMES: [&str; 18] = [
    "accepted",
    "base_rejected",
    "collect_failed",
    "duplicate",
    "stored",
    "acked",
    "nothing_to_ack",
    "forgotten",
    "health_accepted",
    "health_stale",
    "health_change",
    "readings",
    "forged_garbage",
    "forged_foreign_key",
    "forged_downlink",
    "forged_flip",
    "forged_truncate",
    "forged_extend",
];

static STATS: [AtomicU64; STAT_NAMES.len()] = [const { AtomicU64::new(0) }; STAT_NAMES.len()];
static STATS_AT_EXIT: Once = Once::new();

unsafe extern "C" {
    fn atexit(callback: extern "C" fn()) -> i32;
}

extern "C" fn print_stats() {
    for (name, n) in STAT_NAMES.iter().zip(&STATS) {
        eprintln!("{name:>20} {}", n.load(Ordering::Relaxed));
    }
}

fn bump(stat: Stat, n: u64) {
    STATS[stat as usize].fetch_add(n, Ordering::Relaxed);
}

const KEY: [u8; 16] = *b"supersecretkey!1";
const FOREIGN_KEY: [u8; 16] = *b"someone-elses-k!";
const SALT: ServerSalt = ServerSalt(0x5A17);
/// Загрузок в истории: мало, чтобы окна пересекались по номерам.
const BOOTS: u8 = 4;

#[derive(Arbitrary, Debug)]
struct ArbReading {
    time:      u64,
    pressure:  Option<i32>,
    baro_temp: Option<i32>,
    co2:       Option<u16>,
    humidity:  Option<i32>,
    scd_temp:  Option<i32>,
}

impl ArbReading {
    fn reading(&self) -> Reading {
        Reading {
            time:      EpochMillis(self.time),
            pressure:  self.pressure.map(CentiPascal),
            baro_temp: self.baro_temp.map(MilliCelsius),
            co2:       self.co2.map(Ppm),
            humidity:  self.humidity.map(MilliPercentRh),
            scd_temp:  self.scd_temp.map(MilliCelsius),
        }
    }
}

#[derive(Arbitrary, Debug)]
enum Plain {
    /// Произвольный открытый текст (дефект отправителя под верным ключом).
    Raw(Vec<u8>),
    /// Выход кодировщика прошивки: доходит до глубоких веток разбора.
    Encoded {
        health:   Option<[u32; SNAPSHOT_WORDS]>,
        readings: Vec<ArbReading>,
    },
}

#[derive(Arbitrary, Debug)]
enum Forgery {
    Garbage(Vec<u8>),
    ForeignKey,
    /// Подлинная датаграмма обратного направления (подтверждение).
    Downlink,
    FlipBit(u16),
    Truncate(u16),
    Extend(u8),
}

#[derive(Arbitrary, Debug)]
enum Event {
    Authentic {
        boot:    u8,
        seq:     u8,
        ack_req: bool,
        /// Запись в базу удалась (иначе `ingested` не зовётся).
        stored:  bool,
        live:    u16,
        plain:   Plain,
    },
    /// Повтор ранее отправленной подлинной датаграммы байт в байт.
    Replay {
        index:  u8,
        stored: bool,
        live:   u16,
    },
    Forged {
        boot:    u8,
        seq:     u8,
        forgery: Forgery,
    },
    Advance {
        secs: u32,
    },
    /// Сутки забывания ± несколько секунд: граница `BOOT_IDLE_FORGET`
    /// случайными `Advance` почти не попадается.
    AdvanceIdle {
        offset_secs: i8,
    },
}

/// Что приёмник обязан помнить о загрузке.
struct BootModel {
    /// Все записанные номера с создания окна.
    stored:     BTreeSet<u32>,
    last_heard: Duration,
}

impl BootModel {
    fn highest(&self) -> Option<u32> {
        self.stored.last().copied()
    }

    /// Окно помнит номер ⇔ он записан и не дальше `WINDOW_BITS` ниже
    /// наибольшего (окно не едет назад ⇒ отмеченный ниже окна так и не виден).
    fn seen(&self, seq: u32) -> bool {
        self.stored.contains(&seq) && self.highest().is_some_and(|h| h - seq <= WINDOW_BITS)
    }

    fn window(&self) -> Option<(u32, u32)> {
        let h = self.highest()?;
        let bits = (0..WINDOW_BITS)
            .filter(|i| h.checked_sub(1 + i).is_some_and(|s| self.stored.contains(&s)))
            .fold(0u32, |acc, i| acc | (1 << i));
        Some((h, bits))
    }
}

struct World {
    server:      Server,
    cipher:      Aes128Gcm,
    now:         Duration,
    boots:       HashMap<BootId, BootModel>,
    next_ack:    AckSeq,
    health:      HealthLog,
    /// Последний принятый журналом снимок: загрузка и поколение.
    last_health: Option<(BootId, u32)>,
    sent:        Vec<(UpHeader, Vec<u8>)>,
}

fn boot_id(b: u8) -> BootId {
    BootId(u32::from(b % BOOTS))
}

fn plaintext(plain: &Plain) -> Vec<u8> {
    match plain {
        Plain::Raw(bytes) => bytes[..bytes.len().min(PLAIN_CAPACITY)].to_vec(),
        Plain::Encoded { health, readings } => {
            let mut buf = [0u8; MAX_DATAGRAM];
            let area = plain_area(&mut buf);
            let base = readings.first().map_or(EpochMillis(0), |r| EpochMillis(r.time));
            let mut enc = Encoder::new(area, base);
            if let Some(words) = health {
                assert!(
                    enc.push_health(&Snapshot(*words)),
                    "health fits an empty datagram"
                );
            }
            for r in readings {
                if !enc.push(&r.reading()) {
                    break;
                }
            }
            let len = enc.plain_len();
            plain_area(&mut buf)[..len].to_vec()
        }
    }
}

fn sealed(cipher: &Aes128Gcm, header: Header, plain: &[u8]) -> Vec<u8> {
    let mut buf = [0u8; MAX_DATAGRAM];
    plain_area(&mut buf)[..plain.len()].copy_from_slice(plain);
    let len = seal(cipher, header, &mut buf, plain.len());
    buf[..len].to_vec()
}

/// Перекодировать показания настоящим кодировщиком (жадно, сколько датаграмм
/// понадобится) и разобрать обратно.
fn reencode(readings: &[Reading]) -> Vec<Reading> {
    let mut out = Vec::new();
    let mut rest = readings;
    while let Some(first) = rest.first() {
        let mut buf = [0u8; MAX_DATAGRAM];
        let mut enc = Encoder::new(plain_area(&mut buf), first.time);
        let taken = rest.iter().take_while(|r| enc.push(r)).count();
        assert!(taken > 0, "an empty datagram fits any reading");
        let len = enc.plain_len();
        let records = decode(&plain_area(&mut buf)[..len]).expect("encoder output has a base time");
        for record in records {
            match record.expect("encoder output decodes") {
                Record::Reading(r) => out.push(r),
                Record::Health(_) => unreachable!("no health was encoded"),
            }
        }
        rest = &rest[taken..];
    }
    out
}

impl World {
    fn new() -> Self {
        let cipher = Aes128Gcm::new(&KEY.into());
        Self {
            server: Server::new(cipher.clone(), SALT),
            cipher,
            now: Duration::ZERO,
            boots: HashMap::new(),
            next_ack: AckSeq::FIRST,
            health: HealthLog::new(),
            last_health: None,
            sent: Vec::new(),
        }
    }

    fn authentic(&mut self, header: UpHeader, mut datagram: Vec<u8>, plain: &[u8], stored: bool, live: u16) {
        let now = self.now;
        let base_ok = decode(plain).is_ok();
        let incoming = match self.server.receive(&mut datagram, now) {
            Ok(incoming) => incoming,
            Err(e) => {
                // Инвариант 3: только неразборчивый `base_time`, состояние не тронуто.
                assert!(
                    matches!(e, ServerError::Codec(_)),
                    "authentic datagram rejected: {e:?}"
                );
                assert!(!base_ok, "decodable base time rejected: {e:?}");
                bump(Stat::BaseRejected, 1);
                return;
            }
        };
        assert!(base_ok, "undecodable base time accepted");
        bump(Stat::Accepted, 1);
        assert_eq!(
            (incoming.boot, incoming.seq, incoming.ack_req),
            (header.boot, header.seq, header.ack_req)
        );

        // Подлинная датаграмма: окна, молчавшие сутки, забываются, своё продлевается.
        let before = self.boots.len();
        self.boots
            .retain(|_, b| now.saturating_sub(b.last_heard) < BOOT_IDLE_FORGET);
        bump(Stat::Forgotten, (before - self.boots.len()) as u64);
        let model = self.boots.entry(header.boot).or_insert(BootModel {
            stored:     BTreeSet::new(),
            last_heard: now,
        });
        model.last_heard = now;
        // Инвариант 4.
        assert_eq!(
            incoming.duplicate,
            model.seen(header.seq.0),
            "duplicate flag for boot {:?} seq {:?}",
            header.boot,
            header.seq
        );
        let duplicate = incoming.duplicate;
        if duplicate {
            bump(Stat::Duplicate, 1);
        }

        match incoming.collect() {
            Ok(contents) => {
                // Инвариант 6.
                assert_eq!(reencode(&contents.readings), contents.readings);
                bump(Stat::Readings, contents.readings.len() as u64);
                if let Some(record) = &contents.health
                    && let Some(snapshot) = Snapshot::from_record(record)
                {
                    self.health_snapshot(header.boot, snapshot);
                }
                if !duplicate && stored {
                    self.server.ingested(header.boot, header.seq, now);
                    let model = self.boots.get_mut(&header.boot).expect("created above");
                    model.stored.insert(header.seq.0);
                    model.last_heard = now;
                    bump(Stat::Stored, 1);
                }
            }
            // Битая датаграмма не пишется и не подтверждается.
            Err(e) => {
                assert!(matches!(e, ServerError::Codec(_)), "collect failed with {e:?}");
                bump(Stat::CollectFailed, 1);
                return;
            }
        }

        if header.ack_req {
            self.ack(header.boot, LiveSecs(live));
        }
    }

    fn ack(&mut self, boot: BootId, live: LiveSecs) {
        let expected = self.boots.get(&boot).and_then(BootModel::window);
        let ack = self.server.ack(boot, live);
        // Инвариант 5.
        assert_eq!(ack.is_some(), expected.is_some(), "ack presence for {boot:?}");
        let (Some(mut ack), Some((highest, bits))) = (ack, expected) else {
            bump(Stat::NothingToAck, 1);
            return;
        };
        bump(Stat::Acked, 1);
        let opened = open_ack(&self.cipher, &mut ack).expect("server acks open with the shared key");
        assert_eq!(
            opened.header,
            DownHeader {
                salt:    SALT,
                ack_seq: self.next_ack,
            }
        );
        self.next_ack = self.next_ack.next();
        assert_eq!(
            opened.ack,
            Ack {
                boot,
                window: RecvWindow::from_parts(Seq(highest), bits),
                live_for: live,
            }
        );
    }

    fn health_snapshot(&mut self, boot: BootId, snapshot: Snapshot) {
        let stale = self
            .last_health
            .is_some_and(|(b, generation)| b == boot && snapshot.generation() <= generation);
        let changes = self.health.accept(boot, snapshot);
        // Инвариант 7.
        assert_eq!(changes.is_none(), stale, "health log ordering");
        let Some(changes) = changes else {
            bump(Stat::HealthStale, 1);
            return;
        };
        bump(Stat::HealthAccepted, 1);
        bump(Stat::HealthChange, changes.len() as u64);
        self.last_health = Some((boot, snapshot.generation()));
        for change in changes {
            assert_eq!(change.now, snapshot.get(change.word));
            assert_ne!(change.was, change.now);
            // Инвариант 1: расшифровка кодов не паникует ни на каком значении.
            let _ = change.to_string();
        }
    }

    fn forged(&mut self, header: UpHeader, forgery: &Forgery) {
        let honest = || sealed(&self.cipher, Header::Up(header), &[0]);
        let stat = match forgery {
            Forgery::Garbage(_) => Stat::ForgedGarbage,
            Forgery::ForeignKey => Stat::ForgedForeignKey,
            Forgery::Downlink => Stat::ForgedDownlink,
            Forgery::FlipBit(_) => Stat::ForgedFlip,
            Forgery::Truncate(_) => Stat::ForgedTruncate,
            Forgery::Extend(_) => Stat::ForgedExtend,
        };
        bump(stat, 1);
        let mut datagram = match forgery {
            Forgery::Garbage(bytes) => bytes.clone(),
            Forgery::ForeignKey => sealed(&Aes128Gcm::new(&FOREIGN_KEY.into()), Header::Up(header), &[0]),
            Forgery::Downlink => {
                let down = DownHeader {
                    salt:    ServerSalt(header.boot.0),
                    ack_seq: AckSeq(header.seq.0.max(1)),
                };
                sealed(&self.cipher, Header::Down(down), &[0; 14])
            }
            Forgery::FlipBit(bit) => {
                let mut d = honest();
                let bit = usize::from(*bit) % (d.len() * 8);
                d[bit / 8] ^= 1 << (bit % 8);
                d
            }
            Forgery::Truncate(n) => {
                let mut d = honest();
                d.truncate(usize::from(*n) % d.len());
                d
            }
            Forgery::Extend(n) => {
                let mut d = honest();
                d.extend(std::iter::repeat_n(0, usize::from(*n) + 1));
                d
            }
        };
        // Инвариант 2. Отсутствие изменений проверяют следующие подлинные
        // события: модель подделку не видела.
        match self.server.receive(&mut datagram, self.now) {
            Err(ServerError::Datagram(_)) => {}
            Err(e) => panic!("forgery reached the decoder: {e:?}"),
            Ok(_) => panic!("forgery accepted: {forgery:?}"),
        }
    }

    fn step(&mut self, event: &Event) {
        match event {
            Event::Authentic {
                boot,
                seq,
                ack_req,
                stored,
                live,
                plain,
            } => {
                let header = UpHeader {
                    boot:    boot_id(*boot),
                    seq:     Seq(u32::from(*seq)),
                    ack_req: *ack_req,
                };
                let plain = plaintext(plain);
                let datagram = sealed(&self.cipher, Header::Up(header), &plain);
                self.sent.push((header, plain.clone()));
                self.authentic(header, datagram, &plain, *stored, *live);
            }
            Event::Replay { index, stored, live } => {
                if self.sent.is_empty() {
                    return;
                }
                let (header, plain) = self.sent[usize::from(*index) % self.sent.len()].clone();
                let datagram = sealed(&self.cipher, Header::Up(header), &plain);
                self.authentic(header, datagram, &plain, *stored, *live);
            }
            Event::Forged { boot, seq, forgery } => {
                let header = UpHeader {
                    boot:    boot_id(*boot),
                    seq:     Seq(u32::from(*seq)),
                    ack_req: true,
                };
                self.forged(header, forgery);
            }
            Event::Advance { secs } => self.now += Duration::from_secs(u64::from(*secs)),
            Event::AdvanceIdle { offset_secs } => {
                let offset = Duration::from_secs(u64::from(offset_secs.unsigned_abs()));
                self.now += if *offset_secs < 0 {
                    BOOT_IDLE_FORGET - offset
                } else {
                    BOOT_IDLE_FORGET + offset
                };
            }
        }
    }
}

fuzz_target!(|events: Vec<Event>| {
    STATS_AT_EXIT.call_once(|| {
        if std::env::var_os("METEO_FUZZ_STATS").is_some() {
            // SAFETY: `print_stats` — `extern "C"` без аргументов, живёт весь процесс.
            unsafe { atexit(print_stats) };
        }
    });
    let mut world = World::new();
    for event in &events {
        world.step(event);
    }
});
