//! DST: реакция NTP-цикла прошивки на Kiss-o'-Death — `ntp_kiss::KissMemory`
//! против модели пула серверов и DNS-резолвера на виртуальных часах.
//!
//! Цикл — как `ntp_sync_loop` в `meteo/src/ntp_client.rs`: DNS даёт один адрес
//! пула; если память `KoD` его не пускает, запроса нет; до первого синка повтор
//! через 5 с, после — раз в 1000 с. Резолвер держит ответ в кеше TTL секунд,
//! так что отвергнутый адрес приходит снова. Серверы пула: честный, всегда
//! целующий (DENY, RSTR или кодом без смысла), ограничивающий частоту (RATE,
//! если с прошлого запроса прошло меньше его порога) и молчащий (таймаут).
//!
//! Инварианты (RFC 5905 §7.4) проверяет независимая модель по трассе обменов:
//! 1. после DENY/RSTR запросов к серверу нет, пока память могла его не
//!    вытеснить (после него `KoD` со смыслом прислали ≥ `KISS_MEMORY` других);
//! 2. после n-го RATE подряд следующий запрос к серверу — не раньше
//!    `min(64 с · 2^(n−1), 2^17 с)` (счёт сбрасывается при возможном вытеснении);
//! 3. если в ответах DNS есть честный или ограничивающий частоту сервер,
//!    время за жизнь получено; без `KoD` цикл не пропускает ни одной попытки.

use core::net::{
    IpAddr,
    Ipv4Addr,
};
use core::time::Duration;
use std::sync::Mutex;

use meteo_core::ntp_kiss::{
    KISS_MEMORY,
    KissMemory,
    RATE_FIRST_INTERVAL,
    RATE_MAX_INTERVAL,
};
use meteo_core::supervisor::Uptime;
use proptest::collection::vec;
use proptest::prelude::{
    Just,
    Strategy,
    prop_oneof,
};
use proptest::test_runner::{
    Config,
    RngSeed,
    TestCaseError,
    TestRunner,
};
use sntpc::KissOfDeathCode;

// Цикл прошивки (`meteo/src/ntp_client.rs`).
const NTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(7);
const NTP_RETRY_DELAY: Duration = Duration::from_secs(5);
const NTP_RESYNC_INTERVAL: Duration = Duration::from_secs(1000);

const ROUND_TRIP: Duration = Duration::from_millis(40);
const LIFE: Duration = Duration::from_hours(72);

#[derive(Clone, Copy, Debug)]
enum Server {
    Honest,
    /// На каждый запрос — `KoD` с этим кодом.
    Kisses(KissOfDeathCode),
    /// RATE, если с прошлого запроса с нашего IP (и поцелованного тоже)
    /// прошло меньше `min_gap`; иначе время. `primed` — сервер слышал наш IP
    /// в момент загрузки (общий NAT, прошлая загрузка): первый запрос тоже RATE.
    RateLimits {
        min_gap: Duration,
        primed:  bool,
    },
    /// Не отвечает: попытка кончается таймаутом.
    Silent,
}

impl Server {
    /// Сервер, с которым цикл рано или поздно синкается.
    const fn serves(self) -> bool {
        matches!(self, Self::Honest | Self::RateLimits { .. })
    }
}

#[derive(Clone, Debug)]
struct Case {
    pool: Vec<Server>,
    /// Ответы DNS по кругу: индекс сервера в пуле.
    dns:  Vec<usize>,
    /// Сколько резолвер отдаёт из кеша прежний ответ.
    ttl:  Duration,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reply {
    Time,
    Kiss(KissOfDeathCode),
    Timeout,
}

#[derive(Clone, Copy, Debug)]
struct Exchange {
    server:   usize,
    sent:     Duration,
    answered: Duration,
    reply:    Reply,
}

/// Реакция цикла на `KoD`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Policy {
    /// Прошивка: `KissMemory`.
    Obeys,
    /// Прежняя прошивка: `KoD` — такой же отказ, как любой другой.
    Ignores,
}

#[derive(Debug, Default)]
struct Life {
    exchanges:  Vec<Exchange>,
    /// Попытки, на которых память `KoD` не пустила к выданному адресу.
    skipped:    u32,
    first_sync: Option<Duration>,
}

fn address(server: usize) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(
        192,
        0,
        2,
        u8::try_from(server).expect("pool under 256 servers"),
    ))
}

fn live(case: &Case, policy: Policy) -> Life {
    let mut memory = KissMemory::new();
    let mut life = Life::default();
    let mut last_request: Vec<Option<Duration>> = case
        .pool
        .iter()
        .map(|server| matches!(server, Server::RateLimits { primed: true, .. }).then_some(Duration::ZERO))
        .collect();
    let mut dns_turn = 0;
    let mut cached: Option<(usize, Duration)> = None;
    let mut now = Duration::ZERO;
    while now < LIFE {
        let server = match cached {
            Some((server, until)) if now < until => server,
            _ => {
                let server = case.dns[dns_turn % case.dns.len()];
                dns_turn += 1;
                cached = Some((server, now + case.ttl));
                server
            }
        };

        let admitted = match policy {
            Policy::Obeys => memory.admit(address(server), Uptime(now)).is_ok(),
            Policy::Ignores => true,
        };
        if admitted {
            let sent = now;
            let reply = match case.pool[server] {
                Server::Honest => Reply::Time,
                Server::Kisses(code) => Reply::Kiss(code),
                Server::RateLimits { min_gap, .. } => {
                    if last_request[server].is_some_and(|last| sent < last + min_gap) {
                        Reply::Kiss(KissOfDeathCode::Rate)
                    } else {
                        Reply::Time
                    }
                }
                Server::Silent => Reply::Timeout,
            };
            last_request[server] = Some(sent);
            now += if reply == Reply::Timeout {
                NTP_REQUEST_TIMEOUT
            } else {
                ROUND_TRIP
            };
            if let Reply::Kiss(code) = reply {
                memory.kissed(address(server), code, Uptime(now));
            }
            life.exchanges.push(Exchange {
                server,
                sent,
                answered: now,
                reply,
            });
            if reply == Reply::Time && life.first_sync.is_none() {
                life.first_sync = Some(now);
            }
        } else {
            life.skipped += 1;
        }

        now += if life.first_sync.is_some() {
            NTP_RESYNC_INTERVAL
        } else {
            NTP_RETRY_DELAY
        };
    }
    life
}

const fn meaningful(code: KissOfDeathCode) -> bool {
    matches!(
        code,
        KissOfDeathCode::Deny | KissOfDeathCode::Rstr | KissOfDeathCode::Rate
    )
}

/// Что модель знает о сервере после его последнего `KoD` со смыслом.
#[derive(Clone, Copy, Debug)]
enum Known {
    Denied { at: Duration },
    RateLimited { at: Duration, rates: u32 },
}

impl Known {
    const fn at(self) -> Duration {
        match self {
            Self::Denied { at } | Self::RateLimited { at, .. } => at,
        }
    }
}

/// Нижняя граница отступа после `rates`-го RATE подряд.
fn rate_interval(rates: u32) -> Duration {
    let doublings = rates.saturating_sub(1).min(32);
    RATE_FIRST_INTERVAL
        .saturating_mul(1 << doublings)
        .min(RATE_MAX_INTERVAL)
}

/// Могла ли память вытеснить `server`, последний раз поцеловавший в `since`:
/// для этого после него `KoD` со смыслом должны прислать `KISS_MEMORY` других.
fn may_be_forgotten(kisses: &[(Duration, usize)], server: usize, since: Duration) -> bool {
    let mut others: Vec<usize> = kisses
        .iter()
        .filter(|(at, other)| *at > since && *other != server)
        .map(|(_, other)| *other)
        .collect();
    others.sort_unstable();
    others.dedup();
    others.len() >= KISS_MEMORY
}

#[derive(Debug, Default)]
struct Seen {
    denied_waits: u32,
    rate_waits:   u32,
    forgotten:    u32,
}

/// Инварианты 1 и 2 по трассе обменов.
fn check(case: &Case, life: &Life) -> Result<Seen, String> {
    let mut known = vec![None::<Known>; case.pool.len()];
    // (момент, сервер) каждого `KoD` со смыслом.
    let mut kisses: Vec<(Duration, usize)> = Vec::new();
    let mut seen = Seen::default();
    for exchange in &life.exchanges {
        let server = exchange.server;
        if let Some(state) = known[server] {
            if may_be_forgotten(&kisses, server, state.at()) {
                known[server] = None;
                seen.forgotten += 1;
            } else {
                match state {
                    Known::Denied { at } => {
                        return Err(format!(
                            "request to server {server} at {:?} after its DENY/RSTR at {at:?}",
                            exchange.sent
                        ));
                    }
                    Known::RateLimited { at, rates } => {
                        let interval = rate_interval(rates);
                        if exchange.sent < at + interval {
                            return Err(format!(
                                "request to server {server} at {:?}, {:?} after its RATE #{rates}; \
                                 must wait {interval:?}",
                                exchange.sent,
                                exchange.sent.abs_diff(at)
                            ));
                        }
                    }
                }
            }
        }

        let Reply::Kiss(code) = exchange.reply else {
            continue;
        };
        if !meaningful(code) {
            continue;
        }
        let at = exchange.answered;
        if let Some(state) = known[server]
            && may_be_forgotten(&kisses, server, state.at())
        {
            known[server] = None;
        }
        known[server] = Some(match (code, known[server]) {
            (KissOfDeathCode::Rate, Some(Known::RateLimited { rates, .. })) => {
                seen.rate_waits += 1;
                Known::RateLimited { at, rates: rates + 1 }
            }
            (KissOfDeathCode::Rate, None) => {
                seen.rate_waits += 1;
                Known::RateLimited { at, rates: 1 }
            }
            // RATE от отказавшего: отказ остаётся.
            (_, _) => {
                seen.denied_waits += 1;
                Known::Denied { at }
            }
        });
        kisses.push((at, server));
    }
    Ok(seen)
}

/// Инвариант 3.
fn check_liveness(case: &Case, life: &Life) -> Result<(), String> {
    let reachable = case.dns.iter().any(|&server| case.pool[server].serves());
    if reachable && life.first_sync.is_none() {
        return Err("a serving server is in the DNS answers, yet no time in a life".into());
    }
    let kissless = case
        .pool
        .iter()
        .all(|server| !matches!(server, Server::Kisses(_) | Server::RateLimits { .. }));
    if kissless && life.skipped > 0 {
        return Err(format!("{} attempts skipped without any KoD", life.skipped));
    }
    Ok(())
}

fn server() -> impl Strategy<Value = Server> {
    prop_oneof![
        3 => Just(Server::Honest),
        2 => Just(Server::Kisses(KissOfDeathCode::Deny)),
        2 => Just(Server::Kisses(KissOfDeathCode::Rstr)),
        1 => Just(Server::Kisses(KissOfDeathCode::Init)),
        1 => Just(Server::Kisses(KissOfDeathCode::Experimental(*b"XFOO"))),
        4 => (1..3000_u64, proptest::bool::ANY).prop_map(|(secs, primed)| Server::RateLimits {
            min_gap: Duration::from_secs(secs),
            primed,
        }),
        1 => Just(Server::Silent),
    ]
}

/// Пул от одного сервера до вдвое большего, чем помнит память.
fn case() -> impl Strategy<Value = Case> {
    vec(server(), 1..=2 * KISS_MEMORY).prop_flat_map(|pool| {
        let len = pool.len();
        (
            Just(pool),
            vec(0..len, 1..64),
            (0..300_u64).prop_map(Duration::from_secs),
        )
            .prop_map(|(pool, dns, ttl)| Case { pool, dns, ttl })
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

#[test]
fn kissed_servers_are_left_alone() {
    let totals = Mutex::new((0_u64, 0_u64, Seen::default(), 0_u64));
    runner(2048, 0x6e74_705f_6b69_7331)
        .run(&case(), |case| {
            let life = live(&case, Policy::Obeys);
            let seen = check(&case, &life).map_err(TestCaseError::fail)?;
            check_liveness(&case, &life).map_err(TestCaseError::fail)?;
            let mut totals = totals.lock().expect("not poisoned");
            totals.0 += life.exchanges.len() as u64;
            totals.1 += u64::from(life.skipped);
            totals.2.denied_waits += seen.denied_waits;
            totals.2.rate_waits += seen.rate_waits;
            totals.2.forgotten += seen.forgotten;
            totals.3 += u64::from(life.first_sync.is_some());
            drop(totals);
            Ok(())
        })
        .unwrap();

    let (requests, skipped, seen, synced) = totals.into_inner().expect("not poisoned");
    println!(
        "requests: {requests}, skipped after KoD: {skipped}, DENY/RSTR: {}, RATE: {}, \
         re-requests after possible eviction: {}, lives synced: {synced}",
        seen.denied_waits, seen.rate_waits, seen.forgotten
    );
    assert!(seen.denied_waits > 1000, "DENY/RSTR exercised: {seen:?}");
    assert!(seen.rate_waits > 1000, "RATE exercised: {seen:?}");
    assert!(skipped > 10_000, "the memory turned addresses away: {skipped}");
    assert!(seen.forgotten > 10, "eviction exercised: {seen:?}");
    assert!(synced > 1000, "lives reach time: {synced}");
}

/// Регрессия: прежний цикл (`KoD` — обычный отказ, повтор через 5 с) нарушает
/// оба инварианта.
#[test]
fn ignoring_kisses_is_caught() {
    let deny = Case {
        pool: vec![Server::Kisses(KissOfDeathCode::Deny), Server::Honest],
        dns:  vec![0, 0, 1],
        ttl:  Duration::ZERO,
    };
    assert!(check(&deny, &live(&deny, Policy::Obeys)).is_ok());
    let error = check(&deny, &live(&deny, Policy::Ignores)).expect_err("a denied server is asked again");
    assert!(error.contains("DENY/RSTR"), "{error}");

    let rate = Case {
        pool: vec![Server::RateLimits {
            min_gap: Duration::from_secs(600),
            primed:  true,
        }],
        dns:  vec![0],
        ttl:  Duration::from_secs(300),
    };
    let obeys = live(&rate, Policy::Obeys);
    assert!(check(&rate, &obeys).is_ok());
    assert!(obeys.first_sync.is_some(), "backs off until the server answers");
    let error = check(&rate, &live(&rate, Policy::Ignores)).expect_err("RATE is not honoured");
    assert!(error.contains("RATE #1"), "{error}");
}

/// Отступ после RATE удваивается до предела и держится на нём; отказ
/// не снимается RATE'ом; коды без смысла не запоминаются.
#[test]
fn rate_interval_doubles_up_to_the_cap() {
    let server = address(1);
    let mut memory = KissMemory::new();
    let mut now = Duration::ZERO;
    for rates in 1..=20 {
        memory.kissed(server, KissOfDeathCode::Rate, Uptime(now));
        let until = now + rate_interval(rates);
        let just_before = until
            .checked_sub(Duration::from_micros(1))
            .expect("the interval is positive");
        assert!(memory.admit(server, Uptime(just_before)).is_err());
        assert!(memory.admit(server, Uptime(until)).is_ok());
        now = until;
    }
    assert_eq!(rate_interval(20), RATE_MAX_INTERVAL);

    memory.kissed(server, KissOfDeathCode::Deny, Uptime(now));
    memory.kissed(server, KissOfDeathCode::Rate, Uptime(now));
    assert!(memory.admit(server, Uptime(now + LIFE)).is_err());

    let other = address(2);
    assert_eq!(memory.kissed(other, KissOfDeathCode::Init, Uptime(now)), None);
    assert!(memory.admit(other, Uptime(now)).is_ok());
}
