//! DST v2: отправитель прошивки против симулированной сети и настоящего
//! приёмника (`server`) на виртуальных часах. Всё детерминировано: случайность
//! — из seed'а, который выдаёт proptest с фиксированным seed'ом прогона.
//!
//! Модель: датчик даёт показание раз в 30 с (иногда со скачком времени назад —
//! NTP); сеть теряет датаграммы (0/10/30 %), дублирует (5 %) и переставляет
//! (задержка 0–3 с); отправка иногда падает на сокете; бывают «аварии Wi-Fi»,
//! в том числе длиннее ёмкости бэклога (вытеснение). Приёмник: окна записи в
//! базу недоступны, процесс перезапускается (новая соль, окна и срок живого
//! режима потеряны), страница живого графика открывается и закрывается.
//! В конце — фаза заживления: сбои выключены, датчик молчит, бэклог обязан
//! опустеть.
//!
//! Инварианты:
//! 1. каждое показание записано в базу хотя бы раз или учтено как вытеснение;
//! 2. значения в базе совпадают с исходными показаниями точно;
//! 3. нет двух датаграмм с одинаковым (направление, id, счётчик) — nonce;
//! 4. живой режим включается не позже `BATCH_INTERVAL + 30 с` после запроса
//!    (без потерь) и гаснет не позже срока, выданного сервером;
//! 5. трафик без потерь: батч ≤ 50 Б/показание, живой ≤ 90 Б/показание (IP).

use core::time::Duration;
use std::collections::{
    BTreeMap,
    BTreeSet,
    HashSet,
};
use std::sync::atomic::{
    AtomicU64,
    Ordering,
};

use aes_gcm::{
    Aes128Gcm,
    KeyInit,
};
use meteo_core::codec::{
    CentiPascal,
    MilliCelsius,
    MilliPercentRh,
    Ppm,
    Reading,
};
use meteo_core::datagram::{
    BootId,
    Header,
    IP_UDP_OVERHEAD,
    ServerSalt,
    parse_header,
};
use meteo_core::sender::{
    BATCH_INTERVAL,
    Backlog,
    Mode,
    Sender,
};
use meteo_core::server::{
    LiveLease,
    Server,
};
use meteo_core::supervisor::Uptime;
use meteo_core::wire::EpochMillis;
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

const SEC: u64 = 1000;
const MIN: u64 = 60 * SEC;
const HOUR: u64 = 60 * MIN;

const READING_PERIOD: u64 = 30 * SEC;
const MAX_DELAY: u64 = 3 * SEC;
/// Срок, на который продлевает живой режим один запрос страницы.
const LIVE_LEASE: Duration = Duration::from_secs(300);
/// Страница живого графика шлёт запрос раз в столько.
const LIVE_POLL: u64 = MIN;
/// Предел фазы заживления.
const HEAL_LIMIT: u64 = 48 * HOUR;
const EPOCH_BASE: u64 = 1_758_000_000_000;
const BOOT: BootId = BootId(0x5EED_B007);

/// SplitMix64: детерминированный и достаточный для выбора сценариев.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }

    fn step(&mut self, spread: i64) -> i64 {
        self.below(2 * spread as u64 + 1) as i64 - spread
    }
}

/// Сколько раз случился каждый сценарий: зелёный прогон доказывает что-то,
/// только если сценарии были.
#[derive(Default)]
struct Coverage {
    lost:              AtomicU64,
    duplicated:        AtomicU64,
    reordered:         AtomicU64,
    send_failures:     AtomicU64,
    ingest_failures:   AtomicU64,
    server_restarts:   AtomicU64,
    server_duplicates: AtomicU64,
    live_on:           AtomicU64,
    live_off:          AtomicU64,
    evictions:         AtomicU64,
    multi_datagram:    AtomicU64,
    ntp_steps_back:    AtomicU64,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone, Copy)]
enum Dest {
    Server,
    Firmware,
}

/// Сбои, которые генерирует прогон. Без них мир — идеальная сеть с задержкой.
#[derive(Clone, Copy)]
struct Chaos {
    loss_pct:      u64,
    dup_pct:       u64,
    send_fail_pct: u64,
    /// Случайные события: аварии Wi-Fi и базы, рестарты, живой режим.
    events:        bool,
    /// Датчик иногда выдаёт экстремальные значения и шаги времени назад.
    wild_values:   bool,
}

const CALM: Chaos = Chaos {
    loss_pct:      0,
    dup_pct:       0,
    send_fail_pct: 0,
    events:        false,
    wild_values:   false,
};

struct World<'c> {
    rng:        Rng,
    cov:        &'c Coverage,
    chaos:      Chaos,
    now:        u64,
    sender:     Sender<'c>,
    server:     Server,
    salt:       u32,
    lease:      LiveLease,
    /// Самый поздний срок живого режима, который сервер когда-либо выдал.
    lease_max:  Duration,
    /// Страница живого графика открыта: следующий запрос в этот момент.
    live_poll:  Option<u64>,
    wifi_down:  bool,
    ingest_up:  bool,
    producing:  bool,
    next_read:  u64,
    next_event: u64,
    /// В полёте: (время прибытия, порядок) → (куда, байты).
    net:        BTreeMap<(u64, u64), (Dest, Vec<u8>)>,
    net_order:  u64,
    max_seq_rx: u32,
    walk:       [i64; 5],
    produced:   BTreeMap<u64, Reading>,
    evicted:    BTreeSet<u64>,
    db:         BTreeMap<u64, Reading>,
    /// Когда показание впервые попало в базу.
    stored_at:  BTreeMap<u64, u64>,
    nonces:     HashSet<(bool, u32, u32)>,
    /// Байты на уровне IP в обе стороны.
    ip_bytes:   u64,
    was_live:   bool,
}

impl<'c> World<'c> {
    fn new(seed: u64, chaos: Chaos, cov: &'c Coverage, backlog: &'c mut Backlog) -> Self {
        Self {
            rng: Rng(seed),
            cov,
            chaos,
            now: 0,
            sender: Sender::new(cipher(), BOOT, Uptime(Duration::ZERO), backlog),
            server: Server::new(cipher(), ServerSalt(1)),
            salt: 1,
            lease: LiveLease::new(),
            lease_max: Duration::ZERO,
            live_poll: None,
            wifi_down: false,
            ingest_up: true,
            producing: true,
            next_read: 10 * SEC,
            next_event: 5 * MIN,
            net: BTreeMap::new(),
            net_order: 0,
            max_seq_rx: 0,
            walk: [10_132_500, 21_500, 800, 45_000, 22_300],
            produced: BTreeMap::new(),
            evicted: BTreeSet::new(),
            db: BTreeMap::new(),
            stored_at: BTreeMap::new(),
            nonces: HashSet::new(),
            ip_bytes: 0,
            was_live: false,
        }
    }

    fn uptime(&self) -> Uptime {
        Uptime(Duration::from_millis(self.now))
    }

    fn server_now(&self) -> Duration {
        Duration::from_millis(self.now)
    }

    /// Датаграмма уходит в сеть: nonce учтён, трафик посчитан, дальше потеря,
    /// дубль и задержка.
    fn transmit(&mut self, dest: Dest, bytes: Vec<u8>) -> Result<(), String> {
        let key = match parse_header(&bytes).map_err(|e| format!("unparsable datagram on the wire: {e:?}"))? {
            Header::Up(h) => (false, h.boot.0, h.seq.0),
            Header::Down(h) => (true, h.salt.0, h.ack_seq.0),
        };
        if !self.nonces.insert(key) {
            return Err(format!("nonce reused: {key:?}"));
        }
        self.ip_bytes += (bytes.len() + IP_UDP_OVERHEAD) as u64;

        if self.rng.chance(self.chaos.loss_pct) {
            bump(&self.cov.lost);
            return Ok(());
        }
        let copies = if self.rng.chance(self.chaos.dup_pct) {
            bump(&self.cov.duplicated);
            2
        } else {
            1
        };
        for _ in 0..copies {
            let at = self.now + self.rng.below(MAX_DELAY + 1);
            self.net_order += 1;
            self.net.insert((at, self.net_order), (dest, bytes.clone()));
        }
        Ok(())
    }

    fn produce(&mut self) {
        let mut t = EPOCH_BASE + self.now;
        let wild = self.chaos.wild_values;
        if wild && self.rng.chance(3) {
            t -= self.rng.below(5 * SEC);
            bump(&self.cov.ntp_steps_back);
        }
        while self.produced.contains_key(&t) {
            t += 1;
        }
        for (v, spread) in self.walk.iter_mut().zip([60, 25, 15, 250, 20]) {
            *v += self.rng.step(spread);
        }
        self.walk[2] = self.walk[2].clamp(400, 5000);
        let mut value = |ch: usize| -> Option<i64> {
            if self.rng.chance(4) {
                return None;
            }
            if wild && self.rng.chance(1) {
                return Some(if self.rng.chance(50) { i64::MIN } else { i64::MAX });
            }
            Some(self.walk[ch])
        };
        let i32_of = |v: i64| v.clamp(i32::MIN.into(), i32::MAX.into()) as i32;
        let mut r = Reading {
            time:      EpochMillis(t),
            pressure:  value(0).map(|v| CentiPascal(i32_of(v))),
            baro_temp: value(1).map(|v| MilliCelsius(i32_of(v))),
            co2:       value(2).map(|v| Ppm(v.clamp(0, u16::MAX.into()) as u16)),
            humidity:  value(3).map(|v| MilliPercentRh(i32_of(v))),
            scd_temp:  value(4).map(|v| MilliCelsius(i32_of(v))),
        };
        // Пустое показание прошивка в очередь не кладёт.
        if !r.has_any_channel() {
            r.co2 = Some(Ppm(self.walk[2] as u16));
        }
        self.produced.insert(t, r);
        if let Some(old) = self.sender.on_reading(r) {
            self.evicted.insert(old.time.0);
            bump(&self.cov.evictions);
        }
    }

    /// Датаграмма дошла до приёмника: порядок как у настоящего — проверка,
    /// запись в базу, отметка после записи, подтверждение по запросу.
    fn server_receive(&mut self, mut bytes: Vec<u8>) -> Result<(), String> {
        let now = self.server_now();
        let inc = self
            .server
            .receive(&mut bytes, now)
            .map_err(|e| format!("server rejected a firmware datagram: {e:?}"))?;
        let (boot, seq, ack_req, duplicate) = (inc.boot, inc.seq, inc.ack_req, inc.duplicate);
        if seq.0 < self.max_seq_rx {
            bump(&self.cov.reordered);
        }
        self.max_seq_rx = self.max_seq_rx.max(seq.0);

        if duplicate {
            bump(&self.cov.server_duplicates);
        } else {
            let readings = inc
                .collect_readings()
                .map_err(|e| format!("datagram {seq:?} does not decode: {e:?}"))?;
            if !self.ingest_up {
                bump(&self.cov.ingest_failures);
                return Ok(());
            }
            if readings.len() > 40 {
                bump(&self.cov.multi_datagram);
            }
            for r in readings {
                let t = r.time.0;
                let original = self
                    .produced
                    .get(&t)
                    .ok_or_else(|| format!("stored a reading never produced: {r:?}"))?;
                if *original != r {
                    return Err(format!("reading at {t} decoded as {r:?}, produced {original:?}"));
                }
                self.db.insert(t, r);
                self.stored_at.entry(t).or_insert(self.now);
            }
            self.server.ingested(boot, seq, now);
        }
        if ack_req {
            let live_for = self.lease.remaining(now);
            if let Some(ack) = self.server.ack(boot, live_for) {
                self.transmit(Dest::Firmware, ack.to_vec())?;
            }
        }
        Ok(())
    }

    fn chaos_event(&mut self) {
        match self.rng.below(10) {
            0 | 1 => self.wifi_down = !self.wifi_down,
            2 | 3 => self.ingest_up = !self.ingest_up,
            4 => {
                self.salt += 1;
                self.server = Server::new(cipher(), ServerSalt(self.salt));
                self.lease = LiveLease::new();
                bump(&self.cov.server_restarts);
            }
            _ => {
                self.live_poll = match self.live_poll {
                    Some(_) => None,
                    None => Some(self.now),
                };
            }
        }
        // Аварии короткие: минуты, не часы.
        self.next_event = self.now + MIN + self.rng.below(20 * MIN);
    }

    fn next_time(&self) -> u64 {
        let deadline = self.sender.deadline().map(|u| u.0.as_millis() as u64);
        [
            self.producing.then_some(self.next_read),
            self.net.keys().next().map(|k| k.0),
            deadline,
            self.chaos.events.then_some(self.next_event),
            self.live_poll,
        ]
        .into_iter()
        .flatten()
        .min()
        .expect("readings or deadlines always pending while running")
        .max(self.now)
    }

    /// Прогнать мир до `end` (или пока `done` не скажет «хватит»).
    fn run_until(&mut self, end: u64, done: impl Fn(&World) -> bool) -> Result<(), String> {
        let mut same_instant = 0u32;
        loop {
            if done(self) {
                return Ok(());
            }
            let t = if self.producing || !self.net.is_empty() || self.sender.deadline().is_some() {
                self.next_time()
            } else {
                end
            };
            if t > end {
                self.now = end;
                return Ok(());
            }
            same_instant = if t == self.now { same_instant + 1 } else { 0 };
            if same_instant > 10_000 {
                return Err(format!("livelock at {} ms", self.now));
            }
            self.now = t;
            self.step()?;
        }
    }

    fn step(&mut self) -> Result<(), String> {
        let now = self.now;
        while let Some(entry) = self.net.first_entry() {
            if entry.key().0 > now {
                break;
            }
            let (dest, mut bytes) = entry.remove();
            match dest {
                Dest::Server => self.server_receive(bytes)?,
                Dest::Firmware => {
                    self.sender
                        .on_datagram(&mut bytes, self.uptime())
                        .map_err(|e| format!("firmware rejected an ack: {e:?}"))?;
                }
            }
        }
        if self.producing && self.next_read <= now {
            self.produce();
            self.next_read += READING_PERIOD;
        }
        if self.chaos.events && self.next_event <= now {
            self.chaos_event();
        }
        if let Some(at) = self.live_poll
            && at <= now
        {
            self.lease.request(self.server_now(), LIVE_LEASE);
            self.lease_max = self.lease_max.max(self.server_now() + LIVE_LEASE);
            self.live_poll = Some(now + LIVE_POLL);
        }

        // Драйвер прошивки: отправить всё, что автомат отдаёт.
        while let Some(dg) = self.sender.poll(self.uptime()) {
            let bytes = dg.to_vec();
            let socket_error = if self.wifi_down {
                self.rng.chance(50)
            } else {
                self.rng.chance(self.chaos.send_fail_pct)
            };
            if socket_error {
                bump(&self.cov.send_failures);
                self.sender.on_send_failed(self.uptime());
                break;
            }
            if self.wifi_down {
                // Ушло в эфир и пропало. Nonce всё равно израсходован.
                self.transmit_lost(bytes)?;
            } else {
                self.transmit(Dest::Server, bytes)?;
            }
        }
        self.check_live_bound()
    }

    fn transmit_lost(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        let saved = self.chaos.loss_pct;
        self.chaos.loss_pct = 100;
        let res = self.transmit(Dest::Server, bytes);
        self.chaos.loss_pct = saved;
        res
    }

    /// Инвариант 4, верхняя граница: живой режим не переживает срок,
    /// выданный сервером, дольше чем на задержку доставки подтверждения.
    fn check_live_bound(&mut self) -> Result<(), String> {
        match self.sender.mode() {
            Mode::Live { until } => {
                if !self.was_live {
                    bump(&self.cov.live_on);
                }
                self.was_live = true;
                let limit = self.lease_max + Duration::from_millis(MAX_DELAY);
                if until.0 > limit {
                    return Err(format!(
                        "live mode until {:?} outlives the server lease {:?}",
                        until.0, self.lease_max
                    ));
                }
            }
            Mode::Batch => {
                if self.was_live {
                    bump(&self.cov.live_off);
                }
                self.was_live = false;
            }
        }
        Ok(())
    }

    /// Сбои выключены, датчик молчит: бэклог обязан уйти в базу.
    fn heal(&mut self) -> Result<(), String> {
        self.chaos = CALM;
        self.wifi_down = false;
        self.ingest_up = true;
        self.producing = false;
        self.live_poll = None;
        let end = self.now + HEAL_LIMIT;
        self.run_until(end, |w| w.sender.backlog_len() == 0 && w.net.is_empty())?;
        if self.sender.backlog_len() > 0 {
            return Err(format!(
                "no progress: {} readings still in the backlog after {} h of a healthy network",
                self.sender.backlog_len(),
                HEAL_LIMIT / HOUR
            ));
        }
        Ok(())
    }

    /// Инварианты 1 и 2 (2 проверен и при каждой записи в базу).
    fn check_conservation(&self) -> Result<(), String> {
        for (t, r) in &self.produced {
            match self.db.get(t) {
                Some(stored) if stored != r => {
                    return Err(format!("reading at {t}: stored {stored:?}, produced {r:?}"));
                }
                Some(_) => {}
                None if self.evicted.contains(t) => {}
                None => {
                    return Err(format!(
                        "reading at {t} lost: neither stored nor evicted ({} produced, {} stored, {} evicted)",
                        self.produced.len(),
                        self.db.len(),
                        self.evicted.len()
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Один прогон DST по seed'у.
fn run(seed: u64, cov: &Coverage) -> Result<(), String> {
    let mut pick = Rng(seed);
    let chaos = Chaos {
        loss_pct:      [0, 10, 30][pick.below(3) as usize],
        dup_pct:       5,
        send_fail_pct: 2,
        events:        true,
        wild_values:   true,
    };
    // Каждый восьмой прогон — авария Wi-Fi длиннее ёмкости бэклога.
    let long_outage = pick.chance(12);
    let duration = if long_outage {
        16 * HOUR
    } else {
        2 * HOUR + pick.below(4 * HOUR)
    };

    let mut backlog = Box::new(Backlog::new());
    let mut world = World::new(pick.next(), chaos, cov, &mut backlog);
    if long_outage {
        world.run_until(HOUR, |_| false)?;
        world.wifi_down = true;
        world.chaos.events = false;
        world.run_until(HOUR + 13 * HOUR, |_| false)?;
        world.wifi_down = false;
        world.chaos.events = true;
        world.next_event = world.now + MIN;
    }
    world.run_until(duration, |_| false)?;
    world.heal()?;
    world.check_conservation()
}

#[test]
fn every_reading_is_stored_or_evicted() {
    let mut runner = TestRunner::new(Config {
        cases: 512,
        rng_seed: RngSeed::Fixed(0x6d65_7465_6f5f_7632),
        failure_persistence: None,
        ..Config::default()
    });
    let cov = Coverage::default();
    runner
        .run(&any::<u64>(), |seed| run(seed, &cov).map_err(TestCaseError::fail))
        .unwrap();

    let scenarios = [
        ("lost datagrams", &cov.lost),
        ("duplicated datagrams", &cov.duplicated),
        ("reordered datagrams", &cov.reordered),
        ("socket send failures", &cov.send_failures),
        ("ingest failures", &cov.ingest_failures),
        ("server restarts", &cov.server_restarts),
        ("duplicates caught by the window", &cov.server_duplicates),
        ("live mode on", &cov.live_on),
        ("live mode off", &cov.live_off),
        ("evictions", &cov.evictions),
        ("multi-datagram backlog drains", &cov.multi_datagram),
        ("NTP steps back", &cov.ntp_steps_back),
    ];
    let seen = |c: &AtomicU64| c.load(Ordering::Relaxed);
    let report: Vec<String> = scenarios
        .iter()
        .map(|(name, c)| format!("{name} {}", seen(c)))
        .collect();
    println!("coverage: {}", report.join(", "));
    for (name, counter) in scenarios {
        assert!(seen(counter) > 0, "scenario never happened: {name}");
    }
}

/// Инвариант 4 без потерь: страница открыта ⇒ живой режим не позже чем через
/// `BATCH_INTERVAL + 30 с`, показания в базе через секунды; закрыта ⇒ батчи
/// не позже конца срока.
#[test]
fn live_mode_follows_the_page() {
    let cov = Coverage::default();
    let mut backlog = Box::new(Backlog::new());
    let mut world = World::new(7, CALM, &cov, &mut backlog);
    world.run_until(HOUR, |_| false).unwrap();
    assert_eq!(world.sender.mode(), Mode::Batch);

    let opened = world.now;
    world.live_poll = Some(opened);
    let on_by = opened + BATCH_INTERVAL.as_millis() as u64 + 30 * SEC;
    world
        .run_until(on_by, |w| matches!(w.sender.mode(), Mode::Live { .. }))
        .unwrap();
    assert!(
        matches!(world.sender.mode(), Mode::Live { .. }),
        "live mode not on {} s after the page opened",
        (world.now - opened) / SEC
    );
    let live_from = world.now;

    world.run_until(live_from + 30 * MIN, |_| false).unwrap();
    assert!(
        matches!(world.sender.mode(), Mode::Live { .. }),
        "live mode dropped while the page is open"
    );
    let late: Vec<u64> = world
        .stored_at
        .iter()
        .filter(|(t, _)| **t >= EPOCH_BASE + live_from + MIN)
        .map(|(t, at)| at - (t - EPOCH_BASE))
        .filter(|lag| *lag > MAX_DELAY)
        .collect();
    assert!(late.is_empty(), "live readings stored late: {late:?} ms");

    world.live_poll = None;
    let last_request = world.lease_max - LIVE_LEASE;
    let off_by = last_request.as_millis() as u64 + LIVE_LEASE.as_millis() as u64 + MAX_DELAY;
    world.run_until(off_by + 1, |_| false).unwrap();
    assert_eq!(world.sender.mode(), Mode::Batch, "live mode outlived the lease");
}

/// Байт на показание на уровне IP в обе стороны за окно `span` после прогрева.
fn traffic_per_reading(live: bool, span: u64) -> f64 {
    let cov = Coverage::default();
    let mut backlog = Box::new(Backlog::new());
    let mut world = World::new(11, CALM, &cov, &mut backlog);
    if live {
        world.live_poll = Some(0);
    }
    world.run_until(10 * MIN, |_| false).unwrap();
    let (bytes0, readings0) = (world.ip_bytes, world.produced.len());
    world.run_until(10 * MIN + span, |_| false).unwrap();
    assert_eq!(
        matches!(world.sender.mode(), Mode::Live { .. }),
        live,
        "wrong mode for the measurement"
    );
    (world.ip_bytes - bytes0) as f64 / (world.produced.len() - readings0) as f64
}

/// Инвариант 5: регрессия экономии трафика ловится тестом.
#[test]
fn traffic_budget() {
    let batch = traffic_per_reading(false, 24 * HOUR);
    let live = traffic_per_reading(true, 2 * HOUR);
    println!("traffic: batch {batch:.1} B/reading, live {live:.1} B/reading (IP level, both directions)");
    assert!(batch <= 50.0, "batch mode: {batch:.1} B/reading");
    assert!(live <= 90.0, "live mode: {live:.1} B/reading");
}
