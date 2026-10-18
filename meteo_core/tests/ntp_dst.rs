//! DST: NTP-клиент прошивки — настоящий sntpc с локальными часами
//! `ntp::BootClock` — против модели сервера на виртуальных часах. Всё
//! детерминировано: proptest с фиксированным seed'ом, сокет никогда не ждёт.
//!
//! Модель: устройство грузится в истинный момент между вшитым временем и
//! 2090 г. и живёт до 40 лет без перезагрузки, синкаясь с промежутками до
//! 10 лет (каждый следующий синк опирается на результат предыдущего, как в
//! прошивке). Сеть: задержки туда и обратно независимы (асимметрия), сервер
//! тратит время на обработку. Истинное время пересекает эру NTP (2036-02-07)
//! и 2^31 с Unix (2038-01-19).
//!
//! Инварианты:
//! 1. успешный синк даёт epoch загрузки с ошибкой не больше половины
//!    асимметрии пути (+ округление меток);
//! 2. ответ, которому нельзя верить (`KoD`, LI=3, чужой адрес, ответ на прежний
//!    запрос, обрезанный пакет, нулевая метка отправки, режим клиента,
//!    stratum 16, время раньше сборки прошивки), не даёт нового epoch;
//! 3. часы «uptime как Unix-время» (`sntpc_time_embassy`) после 2038 ошибаются
//!    на эру — почему прошивка на них не опирается.

use core::future::{
    self,
    Future,
};
use core::net::{
    IpAddr,
    Ipv4Addr,
    SocketAddr,
};
use core::pin::pin;
use core::task::{
    Context,
    Poll,
    Waker,
};
use core::time::Duration;
use std::sync::Mutex;
use std::sync::atomic::{
    AtomicU64,
    Ordering,
};

use meteo_core::ntp::{
    BootBeforeBuild,
    BootClock,
    BootEpochMicros,
};
use meteo_core::supervisor::Uptime;
use proptest::collection::vec;
use proptest::prelude::Strategy;
use proptest::sample::select;
use proptest::test_runner::{
    Config,
    RngSeed,
    TestCaseError,
    TestRunner,
};
use sntpc::{
    Error,
    KissOfDeathCode,
    NtpContext,
    NtpResult,
    NtpTimestampGenerator,
    NtpUdpSocket,
    get_time,
};

const SERVER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 123)), 123);
const FOREIGN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 66)), 123);

const MICROS: u64 = 1_000_000;
/// Секунды от 1900-01-01 (эпоха NTP) до 1970-01-01.
const NTP_UNIX_DELTA_S: u64 = 2_208_988_800;
/// Средний григорианский год.
const YEAR_US: u64 = 31_556_952 * MICROS;
/// 2038-01-19 03:14:08 UTC: 2^31 с Unix.
const Y2038_US: u64 = (1 << 31) * MICROS;
/// 2036-02-07 06:28:16 UTC: начало эры 1 NTP.
const NTP_ERA1_US: u64 = ((1 << 32) - NTP_UNIX_DELTA_S) * MICROS;

const fn year_us(year: u64) -> u64 {
    (year - 1970) * YEAR_US
}

/// Unix-мкс → 32.32-метка NTP; эра отброшена, как на проводе.
const fn ntp_wire(unix_us: u64) -> u64 {
    let secs = (unix_us / MICROS + NTP_UNIX_DELTA_S) & 0xffff_ffff;
    let fraction = ((unix_us % MICROS) << 32) / MICROS;
    (secs << 32) | fraction
}

/// Будущие сокета модели готовы сразу: опрос один раз.
fn ready<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the simulated socket never blocks"),
    }
}

/// Задержки одного обмена.
#[derive(Clone, Copy, Debug)]
struct Path {
    outbound:   Duration,
    processing: Duration,
    back:       Duration,
}

impl Path {
    const fn asymmetry(self) -> Duration {
        self.outbound.abs_diff(self.back)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    /// Kiss-o'-Death с этим кодом в reference id.
    KissOfDeath([u8; 4]),
    Unsynchronized,
    ForeignSource,
    StaleOrigin,
    Truncated,
    ZeroTransmit,
    ClientMode,
    Stratum16,
    BeforeBuild,
}

// Коды KoD — те, на которые реагирует `ntp_kiss::KissMemory`.
const FAULTS: [Fault; 11] = [
    Fault::KissOfDeath(*b"RATE"),
    Fault::KissOfDeath(*b"DENY"),
    Fault::KissOfDeath(*b"RSTR"),
    Fault::Unsynchronized,
    Fault::ForeignSource,
    Fault::StaleOrigin,
    Fault::Truncated,
    Fault::ZeroTransmit,
    Fault::ClientMode,
    Fault::Stratum16,
    Fault::BeforeBuild,
];

/// Устройство (uptime) и сервер, знающий истинное время.
struct World {
    uptime_us:    AtomicU64,
    true_boot_us: u64,
}

impl World {
    fn advance(&self, by: Duration) {
        let by_us = u64::try_from(by.as_micros()).expect("simulated steps fit u64 microseconds");
        self.uptime_us.fetch_add(by_us, Ordering::Relaxed);
    }

    fn uptime(&self) -> Uptime {
        Uptime(Duration::from_micros(self.uptime_us.load(Ordering::Relaxed)))
    }

    fn true_now_us(&self) -> u64 {
        self.true_boot_us + self.uptime_us.load(Ordering::Relaxed)
    }
}

/// Сокет к серверу модели: запрос → (задержка туда) → метки сервера →
/// (обработка) → ответ → (задержка обратно) → `recv_from`.
struct SimSocket<'w> {
    world: &'w World,
    path:  Path,
    fault: Option<Fault>,
    /// Ответ в пути: байты, длина, источник.
    reply: Mutex<Option<([u8; 48], usize, SocketAddr)>>,
}

impl SimSocket<'_> {
    fn server_now_us(&self) -> u64 {
        match self.fault {
            // Пакет корректен, но сервер уверен, что сейчас 2020 г.
            Some(Fault::BeforeBuild) => year_us(2020) + self.world.uptime_us.load(Ordering::Relaxed),
            _ => self.world.true_now_us(),
        }
    }

    fn answer(&self, origin: u64) -> ([u8; 48], usize, SocketAddr) {
        self.world.advance(self.path.outbound);
        let t2 = self.server_now_us();
        self.world.advance(self.path.processing);
        let t3 = self.server_now_us();

        let (mut leap, mut mode, mut stratum, mut ref_id) = (0_u8, 4_u8, 2_u8, *b"GPS\0");
        let (mut origin, mut tx, mut len, mut source) = (origin, ntp_wire(t3), 48, SERVER);
        match self.fault {
            None | Some(Fault::BeforeBuild) => {}
            Some(Fault::KissOfDeath(code)) => (leap, stratum, ref_id) = (3, 0, code),
            Some(Fault::Unsynchronized) => leap = 3,
            Some(Fault::ForeignSource) => source = FOREIGN,
            // Опоздавший ответ на запрос предыдущей попытки (5 с назад).
            Some(Fault::StaleOrigin) => origin = origin.wrapping_sub(5 << 32),
            Some(Fault::Truncated) => len = 47,
            Some(Fault::ZeroTransmit) => tx = 0,
            Some(Fault::ClientMode) => mode = 3,
            Some(Fault::Stratum16) => stratum = 16,
        }

        let mut packet = [0_u8; 48];
        packet[0] = (leap << 6) | (4 << 3) | mode;
        packet[1] = stratum;
        packet[2] = 6;
        packet[3] = (-20_i8).cast_unsigned();
        // Root delay и dispersion ≈ 3.9 мс (16.16 с).
        packet[4..8].copy_from_slice(&0x100_u32.to_be_bytes());
        packet[8..12].copy_from_slice(&0x100_u32.to_be_bytes());
        packet[12..16].copy_from_slice(&ref_id);
        packet[16..24].copy_from_slice(&ntp_wire(t2 - MICROS).to_be_bytes());
        packet[24..32].copy_from_slice(&origin.to_be_bytes());
        packet[32..40].copy_from_slice(&ntp_wire(t2).to_be_bytes());
        packet[40..48].copy_from_slice(&tx.to_be_bytes());
        (packet, len, source)
    }
}

/// Сокет модели не ждёт: каждая операция завершается при вызове.
impl NtpUdpSocket for SimSocket<'_> {
    fn send_to(&self, buf: &[u8], addr: SocketAddr) -> impl Future<Output = sntpc::Result<usize>> {
        assert_eq!(addr, SERVER, "sntpc sends to the requested server");
        let request: &[u8; 48] = buf.try_into().expect("an SNTP request is 48 bytes");
        let origin = u64::from_be_bytes(request[40..48].try_into().expect("8 bytes"));
        let previous = self
            .reply
            .lock()
            .expect("not poisoned")
            .replace(self.answer(origin));
        assert!(previous.is_none(), "one request per exchange");
        future::ready(Ok(buf.len()))
    }

    fn recv_from(&self, buf: &mut [u8]) -> impl Future<Output = sntpc::Result<(usize, SocketAddr)>> {
        let (packet, len, source) = self
            .reply
            .lock()
            .expect("not poisoned")
            .take()
            .expect("sntpc reads the reply after sending");
        self.world.advance(self.path.back);
        buf[..len].copy_from_slice(&packet[..len]);
        future::ready(Ok((len, source)))
    }
}

/// Один обмен с часами прошивки; `Ok` — новый epoch загрузки.
fn sync(
    world: &World,
    pivot: BootEpochMicros,
    path: Path,
    fault: Option<Fault>,
) -> Result<BootEpochMicros, Rejected> {
    let clock = BootClock::new(pivot, || world.uptime());
    let socket = SimSocket {
        world,
        path,
        fault,
        reply: Mutex::new(None),
    };
    let result = ready(get_time(SERVER, &socket, NtpContext::new(clock))).map_err(Rejected::Sntpc)?;
    clock.synced(&result).map_err(Rejected::Clock)
}

#[derive(Debug, PartialEq)]
enum Rejected {
    Sntpc(Error),
    Clock(BootBeforeBuild),
}

#[derive(Clone, Debug)]
struct Life {
    true_boot_us:    u64,
    first_uptime_us: u64,
    first_path:      Path,
    /// (промежуток до синка, путь).
    resyncs:         Vec<(Duration, Path)>,
}

fn path() -> impl Strategy<Value = Path> {
    (0..2 * MICROS, 0..MICROS, 0..2 * MICROS).prop_map(|(outbound, processing, back)| {
        Path {
            outbound:   Duration::from_micros(outbound),
            processing: Duration::from_micros(processing),
            back:       Duration::from_micros(back),
        }
    })
}

fn life() -> impl Strategy<Value = Life> {
    let built_in = BootEpochMicros::BUILT_IN.as_micros();
    (
        built_in..year_us(2090),
        0..10 * 86_400 * MICROS,
        path(),
        vec((1..10 * YEAR_US, path()), 0..5),
    )
        .prop_map(|(true_boot_us, first_uptime_us, first_path, resyncs)| {
            Life {
                true_boot_us,
                first_uptime_us,
                first_path,
                resyncs: resyncs
                    .into_iter()
                    .map(|(gap_us, path)| (Duration::from_micros(gap_us), path))
                    .collect(),
            }
        })
}

fn runner(cases: u32, seed: u64) -> TestRunner {
    TestRunner::new(Config {
        cases,
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: None,
        ..Config::default()
    })
}

fn check_accuracy(world: &World, synced: BootEpochMicros, path: Path) -> Result<(), TestCaseError> {
    let error_us = synced.as_micros().abs_diff(world.true_boot_us);
    // Смещение = истина + (туда − обратно)/2; метки округляются до мкс и 2^-32 с.
    let bound_us = u64::try_from((path.asymmetry() / 2).as_micros()).expect("asymmetry under 2 s") + 3;
    if error_us > bound_us {
        return Err(TestCaseError::fail(format!(
            "boot epoch off by {error_us} us > {bound_us} us at true time {} us",
            world.true_now_us()
        )));
    }
    Ok(())
}

/// Инвариант 1 на всей жизни устройства: первый синк от вшитого времени,
/// дальше — от предыдущего результата.
#[test]
fn every_sync_recovers_the_boot_epoch() {
    let syncs = AtomicU64::new(0);
    let after_2038 = AtomicU64::new(0);
    let across_era = AtomicU64::new(0);
    runner(4096, 0x6e74_705f_6473_7431)
        .run(&life(), |life| {
            let world = World {
                uptime_us:    AtomicU64::new(life.first_uptime_us),
                true_boot_us: life.true_boot_us,
            };
            let mut boot = sync(&world, BootEpochMicros::BUILT_IN, life.first_path, None)
                .map_err(|rejected| TestCaseError::fail(format!("first sync rejected: {rejected:?}")))?;
            check_accuracy(&world, boot, life.first_path)?;
            syncs.fetch_add(1, Ordering::Relaxed);
            for (gap, path) in life.resyncs {
                let before = world.true_now_us();
                world.advance(gap);
                boot = sync(&world, boot, path, None)
                    .map_err(|rejected| TestCaseError::fail(format!("resync rejected: {rejected:?}")))?;
                check_accuracy(&world, boot, path)?;
                syncs.fetch_add(1, Ordering::Relaxed);
                if world.true_now_us() > Y2038_US {
                    after_2038.fetch_add(1, Ordering::Relaxed);
                }
                if before < NTP_ERA1_US && world.true_now_us() > NTP_ERA1_US {
                    across_era.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        })
        .unwrap();

    let (syncs, after_2038, across_era) = (
        syncs.into_inner(),
        after_2038.into_inner(),
        across_era.into_inner(),
    );
    println!("syncs: {syncs}, after 2038: {after_2038}, across the NTP era: {across_era}");
    assert!(after_2038 > 1000, "the model must reach past 2038: {after_2038}");
    assert!(
        across_era > 100,
        "the model must resync across the NTP era: {across_era}"
    );
}

/// Инвариант 2: каждому отказу — своя причина, нового epoch нет.
#[test]
fn untrusted_replies_do_not_move_the_clock() {
    let seen = Mutex::new([0_u32; FAULTS.len()]);
    let case = (
        BootEpochMicros::BUILT_IN.as_micros()..year_us(2090),
        0..YEAR_US,
        path(),
        select(&FAULTS[..]),
    );
    runner(2048, 0x6e74_705f_6473_7432)
        .run(&case, |(true_boot_us, uptime_us, path, fault)| {
            let world = World {
                uptime_us: AtomicU64::new(uptime_us),
                true_boot_us,
            };
            let outcome = sync(&world, BootEpochMicros::BUILT_IN, path, Some(fault));
            let sntpc_error = |error| outcome == Err(Rejected::Sntpc(error));
            let rejected_as_expected = match fault {
                Fault::KissOfDeath(code) => {
                    let expected = match &code {
                        b"RATE" => KissOfDeathCode::Rate,
                        b"DENY" => KissOfDeathCode::Deny,
                        b"RSTR" => KissOfDeathCode::Rstr,
                        _ => unreachable!("FAULTS lists only these codes"),
                    };
                    sntpc_error(Error::KissOfDeath(expected))
                }
                Fault::Unsynchronized => sntpc_error(Error::UnsynchronizedClock),
                Fault::ForeignSource => sntpc_error(Error::ResponseAddressMismatch),
                Fault::StaleOrigin => sntpc_error(Error::IncorrectOriginTimestamp),
                Fault::Truncated => sntpc_error(Error::IncorrectPayload),
                Fault::ZeroTransmit => sntpc_error(Error::InvalidTimestamp),
                Fault::ClientMode => sntpc_error(Error::IncorrectMode),
                Fault::Stratum16 => sntpc_error(Error::IncorrectStratumHeaders),
                // Пакет валиден для sntpc — отвергают часы.
                Fault::BeforeBuild => matches!(outcome, Err(Rejected::Clock(_))),
            };
            if !rejected_as_expected {
                return Err(TestCaseError::fail(format!("{fault:?}: {outcome:?}")));
            }
            let index = FAULTS
                .iter()
                .position(|f| *f == fault)
                .expect("fault from FAULTS");
            seen.lock().expect("not poisoned")[index] += 1;
            Ok(())
        })
        .unwrap();

    let seen = seen.into_inner().expect("not poisoned");
    println!(
        "faults exercised: {:?}",
        FAULTS.iter().zip(seen).collect::<Vec<_>>()
    );
    assert!(
        seen.iter().all(|count| *count > 50),
        "every fault exercised: {seen:?}"
    );
}

/// Часы `sntpc_time_embassy::EmbassyTimestampGenerator`: uptime как Unix-время.
#[derive(Clone, Copy)]
struct UptimeAsUnix<'w> {
    world:  &'w World,
    now_us: u64,
}

impl NtpTimestampGenerator for UptimeAsUnix<'_> {
    fn init(&mut self) {
        self.now_us = self.world.uptime_us.load(Ordering::Relaxed);
    }

    fn timestamp_sec(&self) -> u64 {
        self.now_us / MICROS
    }

    fn timestamp_subsec_micros(&self) -> u32 {
        u32::try_from(self.now_us % MICROS).expect("< 10^6")
    }
}

/// Epoch загрузки по старой схеме прошивки: `offset()` от часов-uptime.
fn boot_epoch_from_uptime_clock(true_boot_us: u64) -> i64 {
    let world = World {
        uptime_us: AtomicU64::new(86_400 * MICROS),
        true_boot_us,
    };
    let path = Path {
        outbound:   Duration::from_millis(20),
        processing: Duration::from_micros(100),
        back:       Duration::from_millis(20),
    };
    let socket = SimSocket {
        world: &world,
        path,
        fault: None,
        reply: Mutex::new(None),
    };
    let clock = UptimeAsUnix {
        world:  &world,
        now_us: 0,
    };
    let result: NtpResult = ready(get_time(SERVER, &socket, NtpContext::new(clock))).expect("a valid reply");
    result.offset()
}

/// Инвариант 3: окно смещения sntpc — ±2^31 с от локальных часов. От часов-
/// uptime (1970) оно кончается в 2038 и смещение молча уходит на эру назад.
#[test]
fn uptime_clock_breaks_after_2038() {
    let before = year_us(2030);
    let error_before = boot_epoch_from_uptime_clock(before).abs_diff(before.cast_signed());
    assert!(
        error_before < MICROS,
        "fine before 2038: off by {error_before} us"
    );

    let after = year_us(2040);
    let error_after = boot_epoch_from_uptime_clock(after).abs_diff(after.cast_signed());
    assert!(
        error_after > 100 * YEAR_US,
        "wraps after 2038: off by {error_after} us"
    );
}

/// Момент загрузки пересчитывается в wall-clock с точностью до мс и насыщается,
/// а не переполняется.
#[test]
fn epoch_at_adds_uptime_and_saturates() {
    let boot = BootEpochMicros::BUILT_IN;
    let at = Uptime(Duration::from_micros(1_500_999));
    assert_eq!(boot.epoch_at(at).0, (boot.as_micros() + 1_500_999) / 1000);
    assert_eq!(boot.epoch_at(Uptime(Duration::MAX)).0, u64::MAX / 1000);
}
