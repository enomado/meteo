//! Журнал здоровья: счётчики сбоев, причина загрузки, место зависания.
//!
//! Живёт в памяти, которая переживает reset (у прошивки — RTC FAST,
//! `#[ram(rtc_fast, persistent)]`), и уходит на сервер снимком в датаграмме v2.
//!
//! Счётчики, а не лента событий: объём фиксирован, переполнения нет, повторы
//! склеиваются сами, а доставка идемпотентна — свежий снимок вытесняет
//! предыдущий. Время есть у «последнего сбоя» датчика и у зависания; у
//! приращений — с точностью до батча, в котором пришёл снимок.
//!
//! Раскладка — одно слово `u32` на поле. Первое слово — магия с версией
//! раскладки: на холодном старте память — мусор, и чужая магия обнуляет журнал.
//! Новое поле добавляется только в конец, с увеличением [`LAYOUT`].
//!
//! Конкурентность: пишут таски одного executor'а (вытеснения между ними нет),
//! прерывания журнал не трогают. Атомики — потому что область `static` и
//! переживает reset (`Persistable` есть у атомиков), а не ради гонок.

use core::time::Duration;

use portable_atomic::{
    AtomicU32,
    Ordering,
};

use crate::codec::HealthRecord;
use crate::sensors::{
    Fault,
    OpCode,
    SensorId,
};
use crate::supervisor::{
    BootFault,
    ResetKind,
    Uptime,
    classify,
};

/// Версия раскладки слов. Меняется при любом изменении состава или порядка.
pub const LAYOUT: u32 = 1;
/// "mh" + версия раскладки: отличает журнал от мусора холодного старта.
const MAGIC: u32 = 0x6D68_0000 | LAYOUT;

/// Как слово ведёт себя при записи и уходит ли на сервер.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Магия раскладки. Не передаётся.
    Magic,
    /// Растёт на каждое изменение передаваемых слов: приёмник упорядочивает
    /// снимки одной сессии питания, отправитель понимает, что снимок устарел.
    Generation,
    /// Счётчик с подачи питания, насыщается на `u32::MAX`.
    Counter,
    /// Последнее значение (код, время, этап).
    Value,
    /// Служебное слово прошивки: меняется постоянно (этап таски, отметка
    /// жизни), поколение не трогает и на сервер не уходит.
    Live,
}

macro_rules! words {
    ($($(#[$meta:meta])* $name:ident: $kind:ident = $wire:literal;)*) => {
        /// Поле журнала. Порядок объявления = индекс слова = порядок на проводе.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Word {
            $($(#[$meta])* $name,)*
        }

        /// Всего слов в журнале (вместе с магией и служебными).
        pub const WORDS: usize = [$($wire),*].len();

        impl Word {
            /// Все поля в порядке раскладки.
            pub const ALL: [Self; WORDS] = [$(Self::$name,)*];

            #[must_use]
            pub const fn kind(self) -> Kind {
                match self {
                    $(Self::$name => Kind::$kind,)*
                }
            }

            /// Имя для журнала приёмника.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$name => $wire,)*
                }
            }
        }
    };
}

words! {
    Magic: Magic = "magic";
    Generation: Generation = "generation";

    BootTotal: Counter = "boot.total";
    BootPanic: Counter = "boot.panic";
    BootWdt: Counter = "boot.wdt";
    BootOther: Counter = "boot.other";
    /// Сырой код `SocResetReason` последней загрузки; 0 — код не описан в esp-hal.
    BootLastReset: Value = "boot.last_reset";

    /// Биты [`STALL_SENSOR`], [`STALL_NET`], [`STALL_EXECUTOR`]; 0 — зависаний не было.
    StallTasks: Value = "stall.tasks";
    /// Код этапа sensor-таски в момент зависания ([`OpCode`], 0 — ожидание).
    StallSensorStage: Value = "stall.sensor_stage";
    /// Код этапа сетевой таски ([`NetStage`]).
    StallNetStage: Value = "stall.net_stage";
    /// Uptime зависшей загрузки, с.
    StallUptimeS: Value = "stall.uptime_s";
    /// Epoch зависания, с; 0 — часы той загрузки не были синхронизированы.
    StallEpochS: Value = "stall.epoch_s";

    Scd41Timeout: Counter = "scd41.timeout";
    Scd41Bus: Counter = "scd41.bus";
    Scd41Crc: Counter = "scd41.crc";
    Scd41Rejected: Counter = "scd41.rejected";
    Scd41Stale: Counter = "scd41.stale";
    Scd41BadValue: Counter = "scd41.bad_value";
    Scd41Reinit: Counter = "scd41.reinit";
    Scd41ReinitFailed: Counter = "scd41.reinit_failed";
    Scd41LastOp: Value = "scd41.last_op";
    Scd41LastFault: Value = "scd41.last_fault";
    Scd41LastEpochS: Value = "scd41.last_epoch_s";

    Bmp390Timeout: Counter = "bmp390.timeout";
    Bmp390Bus: Counter = "bmp390.bus";
    Bmp390Crc: Counter = "bmp390.crc";
    Bmp390Rejected: Counter = "bmp390.rejected";
    Bmp390Stale: Counter = "bmp390.stale";
    Bmp390BadValue: Counter = "bmp390.bad_value";
    Bmp390Reinit: Counter = "bmp390.reinit";
    Bmp390ReinitFailed: Counter = "bmp390.reinit_failed";
    Bmp390LastOp: Value = "bmp390.last_op";
    Bmp390LastFault: Value = "bmp390.last_fault";
    Bmp390LastEpochS: Value = "bmp390.last_epoch_s";

    WifiDisconnects: Counter = "wifi.disconnects";
    WifiConnectFailures: Counter = "wifi.connect_failures";
    /// Код причины последнего обрыва Wi-Fi от драйвера радио.
    WifiLastReason: Value = "wifi.last_reason";
    NtpFailures: Counter = "ntp.failures";
    /// Запрос подтверждения остался без ответа (таймаут или сбой отправки).
    AckFailures: Counter = "net.ack_failures";
    QueueEvictions: Counter = "net.queue_evictions";
    BacklogEvictions: Counter = "net.backlog_evictions";

    /// `custom_halt` отметил панику перед reset.
    PanicMark: Live = "live.panic_mark";
    /// Watchdog записал зависание и не кормит RWDT.
    StallPending: Live = "live.stall_pending";
    SensorStage: Live = "live.sensor_stage";
    NetStage: Live = "live.net_stage";
    /// Uptime последнего тика watchdog'а, с: время смерти для зависания всего executor'а.
    AliveUptimeS: Live = "live.alive_uptime_s";
    /// Epoch момента boot, с (`epoch − uptime`); 0 — часы не синхронизированы.
    ClockOffsetS: Live = "live.clock_offset_s";
}

/// `StallTasks`: встала sensor-таска.
pub const STALL_SENSOR: u32 = 1 << 0;
/// `StallTasks`: встала сетевая таска.
pub const STALL_NET: u32 = 1 << 1;
/// `StallTasks`: watchdog не успел записать зависание — встал весь executor.
pub const STALL_EXECUTOR: u32 = 1 << 2;

/// Сколько слов уходит на сервер (поколение, счётчики, значения).
pub const SNAPSHOT_WORDS: usize = {
    let mut n = 0;
    let mut i = 0;
    while i < WORDS {
        if Word::ALL[i].transmitted() {
            n += 1;
        }
        i += 1;
    }
    n
};

impl Word {
    #[must_use]
    pub const fn transmitted(self) -> bool {
        matches!(self.kind(), Kind::Generation | Kind::Counter | Kind::Value)
    }

    const fn index(self) -> usize {
        self as usize
    }

    /// Поля снимка в порядке провода.
    pub fn snapshot_fields() -> impl Iterator<Item = Self> {
        Self::ALL.into_iter().filter(|w| w.transmitted())
    }
}

/// Этап сетевой таски: что она делала, когда встала.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum NetStage {
    /// Ждёт сеть (link/DHCP) до первого сокета.
    WaitLink = 1,
    /// Перекладывает показания из передаточной очереди.
    TakeReadings = 2,
    /// Отправка датаграммы.
    Send = 3,
    /// Ждёт подтверждение или дедлайн отправителя.
    Wait = 4,
}

impl NetStage {
    #[must_use]
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// Имя по коду из журнала; 0 — этап не записан.
    #[must_use]
    pub const fn name_of(code: u32) -> &'static str {
        match code {
            0 => "none",
            1 => "wait_link",
            2 => "take_readings",
            3 => "send",
            4 => "wait",
            _ => "unknown",
        }
    }
}

/// Что сделал [`Health::attach`] с памятью.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// Магия на месте: журнал продолжен.
    Continued,
    /// Подача питания или мусор: журнал обнулён.
    Fresh,
}

/// Снимок передаваемых слов в порядке провода.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot(pub [u32; SNAPSHOT_WORDS]);

impl Snapshot {
    /// Снимок своей раскладки из записи датаграммы; `None` — раскладка или
    /// число слов чужие (прошивка другой версии).
    #[must_use]
    pub fn from_record(record: &HealthRecord) -> Option<Self> {
        if record.layout != LAYOUT {
            return None;
        }
        record.words.as_slice().try_into().ok().map(Self)
    }

    #[must_use]
    pub fn generation(&self) -> u32 {
        self.get(Word::Generation)
    }

    /// Значение поля; для непередаваемого — 0.
    #[must_use]
    pub fn get(&self, word: Word) -> u32 {
        Word::snapshot_fields()
            .position(|w| w == word)
            .map_or(0, |i| self.0[i])
    }

    /// Поля, изменившиеся относительно `prev`, кроме поколения:
    /// `(поле, было, стало)`. `prev = None` — все ненулевые поля.
    pub fn changes<'a>(&'a self, prev: Option<&'a Self>) -> impl Iterator<Item = (Word, u32, u32)> + 'a {
        Word::snapshot_fields()
            .zip(self.0)
            .enumerate()
            .filter(|(_, (w, _))| *w != Word::Generation)
            .filter_map(move |(i, (w, now))| {
                let was = prev.map_or(0, |p| p.0[i]);
                (was != now).then_some((w, was, now))
            })
    }
}

/// Журнал поверх слов, переживающих reset.
pub struct Health<'a> {
    words: &'a [AtomicU32; WORDS],
}

impl<'a> Health<'a> {
    /// Подключиться к памяти журнала на буте. `power_on` — reset по подаче
    /// питания: память — мусор, даже если магия случайно совпала.
    pub fn attach(words: &'a [AtomicU32; WORDS], power_on: bool) -> (Self, Attach) {
        let health = Self { words };
        if !power_on && health.load(Word::Magic) == MAGIC {
            return (health, Attach::Continued);
        }
        for word in words {
            word.store(0, Ordering::Relaxed);
        }
        health.store(Word::Magic, MAGIC);
        (health, Attach::Fresh)
    }

    /// Журнал, уже подключённый на буте [`Health::attach`]: таски и
    /// обработчик паники берут вид на те же слова, не трогая магию.
    #[must_use]
    pub const fn view(words: &'a [AtomicU32; WORDS]) -> Self {
        Self { words }
    }

    fn load(&self, word: Word) -> u32 {
        self.words[word.index()].load(Ordering::Relaxed)
    }

    fn store(&self, word: Word, value: u32) {
        self.words[word.index()].store(value, Ordering::Relaxed);
    }

    fn touch(&self) {
        let generation = self.load(Word::Generation).wrapping_add(1);
        self.store(Word::Generation, generation);
    }

    #[must_use]
    pub fn get(&self, word: Word) -> u32 {
        self.load(word)
    }

    /// `+1` к счётчику.
    pub fn bump(&self, word: Word) {
        self.add(word, 1);
    }

    /// `+n` к счётчику (с насыщением).
    ///
    /// # Panics
    ///
    /// Паникует, если `word` не счётчик: это ошибка вызывающего кода.
    pub fn add(&self, word: Word, n: u32) {
        assert_eq!(word.kind(), Kind::Counter, "{} is not a counter", word.name());
        if n == 0 {
            return;
        }
        self.store(word, self.load(word).saturating_add(n));
        self.touch();
    }

    /// Записать значение; поколение растёт, только если оно изменилось.
    ///
    /// # Panics
    ///
    /// Паникует, если `word` не значение и не служебное слово.
    pub fn set(&self, word: Word, value: u32) {
        match word.kind() {
            Kind::Value => {
                if self.load(word) != value {
                    self.store(word, value);
                    self.touch();
                }
            }
            Kind::Live => self.store(word, value),
            Kind::Magic | Kind::Generation | Kind::Counter => {
                panic!("{} is not settable", word.name())
            }
        }
    }

    /// Часы синхронизированы: `epoch_ms` соответствует моменту `now`.
    pub fn clock_synced(&self, now: Uptime, epoch_ms: u64) {
        let offset_s = (epoch_ms / 1000).saturating_sub(now.0.as_secs());
        self.set(Word::ClockOffsetS, u32::try_from(offset_s).unwrap_or(0));
    }

    /// Epoch момента `now` этой загрузки, с; 0 — часы не синхронизированы.
    #[must_use]
    pub fn epoch_s(&self, now: Uptime) -> u32 {
        epoch_from(self.load(Word::ClockOffsetS), now.0)
    }

    /// Учесть сбой датчика: счётчик вида и «последний сбой».
    pub fn sensor_fault(&self, sensor: SensorId, op: OpCode, fault: Fault, now: Uptime) {
        self.bump(fault_counter(sensor, fault));
        let words = sensor_words(sensor);
        self.set(words.last_op, op.0);
        self.set(words.last_fault, fault.code());
        self.set(words.last_epoch_s, self.epoch_s(now));
    }

    /// Переинициализация датчика завершилась: удалась (`ok`) или нет.
    pub fn sensor_reinit(&self, sensor: SensorId, ok: bool) {
        let words = sensor_words(sensor);
        self.bump(if ok { words.reinit } else { words.reinit_failed });
    }

    /// Классифицировать и учесть текущую загрузку. Зовётся один раз на буте,
    /// до того как таски перепишут служебные слова прошлой загрузки.
    ///
    /// Зависание: watchdog успел записать его сам (`StallPending`) — данные
    /// уже в журнале; иначе встал весь executor, и место — этапы тасок и
    /// последний тик watchdog'а прошлой загрузки.
    #[must_use]
    pub fn record_boot(&self, kind: ResetKind, reset_code: u32) -> BootFault {
        let fault = classify(kind, self.load(Word::PanicMark) != 0);
        self.bump(Word::BootTotal);
        self.set(Word::BootLastReset, reset_code);
        if kind == ResetKind::Other {
            self.bump(Word::BootOther);
        }
        match fault {
            BootFault::Clean => {}
            BootFault::Panic => self.bump(Word::BootPanic),
            BootFault::WdtStall => {
                self.bump(Word::BootWdt);
                if self.load(Word::StallPending) == 0 {
                    let alive = Duration::from_secs(u64::from(self.load(Word::AliveUptimeS)));
                    self.set(Word::StallTasks, STALL_EXECUTOR);
                    self.set(Word::StallSensorStage, self.load(Word::SensorStage));
                    self.set(Word::StallNetStage, self.load(Word::NetStage));
                    self.set(Word::StallUptimeS, self.load(Word::AliveUptimeS));
                    self.set(
                        Word::StallEpochS,
                        epoch_from(self.load(Word::ClockOffsetS), alive),
                    );
                }
            }
        }
        // Служебные слова прошлой загрузки отслужили.
        for word in [
            Word::PanicMark,
            Word::StallPending,
            Word::SensorStage,
            Word::NetStage,
            Word::AliveUptimeS,
            Word::ClockOffsetS,
        ] {
            self.set(word, 0);
        }
        fault
    }

    /// Паника: `custom_halt` отмечает её перед программным reset (по железу
    /// он неотличим от штатного).
    pub fn mark_panic(&self) {
        self.set(Word::PanicMark, 1);
    }

    /// Сколько раз страховки срабатывали с подачи питания.
    #[must_use]
    pub fn faults_since_power_on(&self) -> u32 {
        self.load(Word::BootPanic)
            .saturating_add(self.load(Word::BootWdt))
    }

    /// Watchdog решил не кормить RWDT: записать, кто встал и где.
    pub fn record_stall(&self, sensor_ok: bool, net_ok: bool, now: Uptime) {
        let tasks = if sensor_ok { 0 } else { STALL_SENSOR } | if net_ok { 0 } else { STALL_NET };
        self.set(Word::StallTasks, tasks);
        self.set(Word::StallSensorStage, self.load(Word::SensorStage));
        self.set(Word::StallNetStage, self.load(Word::NetStage));
        self.set(
            Word::StallUptimeS,
            u32::try_from(now.0.as_secs()).unwrap_or(u32::MAX),
        );
        self.set(Word::StallEpochS, self.epoch_s(now));
        self.set(Word::StallPending, 1);
    }

    /// Тик watchdog'а: таски живы в момент `now`.
    pub fn alive(&self, now: Uptime) {
        self.set(
            Word::AliveUptimeS,
            u32::try_from(now.0.as_secs()).unwrap_or(u32::MAX),
        );
    }

    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let mut out = [0u32; SNAPSHOT_WORDS];
        for (slot, word) in out.iter_mut().zip(Word::snapshot_fields()) {
            *slot = self.load(word);
        }
        Snapshot(out)
    }
}

fn epoch_from(offset_s: u32, uptime: Duration) -> u32 {
    if offset_s == 0 {
        return 0;
    }
    let uptime_s = u32::try_from(uptime.as_secs()).unwrap_or(u32::MAX);
    offset_s.saturating_add(uptime_s)
}

/// Слова одного датчика.
struct SensorWords {
    timeout:       Word,
    bus:           Word,
    crc:           Word,
    rejected:      Word,
    stale:         Word,
    bad_value:     Word,
    reinit:        Word,
    reinit_failed: Word,
    last_op:       Word,
    last_fault:    Word,
    last_epoch_s:  Word,
}

const fn sensor_words(sensor: SensorId) -> SensorWords {
    match sensor {
        SensorId::Scd41 => {
            SensorWords {
                timeout:       Word::Scd41Timeout,
                bus:           Word::Scd41Bus,
                crc:           Word::Scd41Crc,
                rejected:      Word::Scd41Rejected,
                stale:         Word::Scd41Stale,
                bad_value:     Word::Scd41BadValue,
                reinit:        Word::Scd41Reinit,
                reinit_failed: Word::Scd41ReinitFailed,
                last_op:       Word::Scd41LastOp,
                last_fault:    Word::Scd41LastFault,
                last_epoch_s:  Word::Scd41LastEpochS,
            }
        }
        SensorId::Bmp390 => {
            SensorWords {
                timeout:       Word::Bmp390Timeout,
                bus:           Word::Bmp390Bus,
                crc:           Word::Bmp390Crc,
                rejected:      Word::Bmp390Rejected,
                stale:         Word::Bmp390Stale,
                bad_value:     Word::Bmp390BadValue,
                reinit:        Word::Bmp390Reinit,
                reinit_failed: Word::Bmp390ReinitFailed,
                last_op:       Word::Bmp390LastOp,
                last_fault:    Word::Bmp390LastFault,
                last_epoch_s:  Word::Bmp390LastEpochS,
            }
        }
    }
}

/// Счётчик вида сбоя датчика.
#[must_use]
pub const fn fault_counter(sensor: SensorId, fault: Fault) -> Word {
    let w = sensor_words(sensor);
    match fault {
        Fault::Timeout => w.timeout,
        Fault::Bus => w.bus,
        Fault::Crc => w.crc,
        Fault::Rejected => w.rejected,
        Fault::Stale => w.stale,
        Fault::BadValue => w.bad_value,
    }
}

/// Счётчики переинициализаций датчика: `(удалась, не удалась)`.
#[must_use]
pub const fn reinit_counters(sensor: SensorId) -> (Word, Word) {
    let w = sensor_words(sensor);
    (w.reinit, w.reinit_failed)
}
