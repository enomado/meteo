//! DST опроса датчиков: автомат `sensors` против модели SCD41 и BMP390 на
//! виртуальных часах. Всё детерминировано: случайность — из seed'а, который
//! выдаёт proptest с фиксированным seed'ом прогона.
//!
//! Модель SCD41: idle/measuring, в low-power periodic первые данные через
//! ~5 с после `start`, дальше раз в 30 ± 1 с, непрочитанные перезаписываются, `stop` сбрасывает буфер, `reinit` грузит
//! смещение из «EEPROM»; после `start` температура выше установившейся и сходит
//! к ней за `SCD_WARMUP` (замер на плате). Модель BMP390: normal-режим
//! начинается с преобразования ⇒ первый `drdy` через время замера после
//! инициализации, дальше раз в 20 с.
//! Драйвер исполняет операцию за её время исполнения; «зависание» — операция
//! длится `op.timeout()` и возвращает `Timeout` (так делает драйвер прошивки).
//!
//! Сбои: случайные зависания, NACK, CRC и мусорные значения с вероятностью
//! на операцию; эпизоды залипания датчика — до `reinit`/инициализации, на
//! время или навсегда.
//!
//! Инварианты:
//! 1. между завершёнными циклами ≤ 60 с (лимит watchdog 90 с) и > 0;
//!    предел каждой операции ≤ `OP_TIMEOUT_MAX`; `Wait` всегда в будущее;
//! 2. легальность по даташиту: во время замера — только разрешённые команды,
//!    чтение — только при готовых данных, после сбоя команда SCD41 не раньше
//!    `SETTLE`, BMP390 опрашивается только инициализированным;
//! 3. показание несёт ровно прочитанное в этом цикле;
//! 4. внедрённые сбои учтены в журнале точно (датчик × вид);
//! 5. без случайных сбоев: после конца залипания данные возвращаются не
//!    позже `RECOVERY_BOUND`; здоровый датчик не получает ни сбоев, ни
//!    переинициализаций, ни `Stale`;
//! 6. мёртвый SCD41 не останавливает BMP390.

use core::time::Duration;
use std::collections::HashMap;
use std::sync::atomic::{
    AtomicU64,
    Ordering,
};

use meteo_core::codec::{
    CentiPascal,
    MilliCelsius,
    MilliPercentRh,
};
use meteo_core::health::{
    Health,
    WORDS,
    Word,
    fault_counter,
    reinit_counters,
};
use meteo_core::sensors::{
    Action,
    BaroRaw,
    Cycle,
    Fault,
    OP_TIMEOUT_MAX,
    Op,
    OpResult,
    SETTLE,
    ScdRaw,
    SensorId,
    Sensors,
};
use meteo_core::supervisor::Uptime;
use portable_atomic::AtomicU32;
use proptest::prelude::any;
use proptest::test_runner::{
    Config,
    RngSeed,
    TestCaseError,
    TestRunner,
};

const SEC: u64 = 1000;
const MIN: u64 = 60 * SEC;
const HOUR: u64 = 60 * MIN;

const SCD_INTERVAL: u64 = 30 * SEC;
/// От `start` low-power до первых данных (плата, 2026-10-18, два старта:
/// данные готовы между 4 и 6 с при опросе раз в 2 с). Не `SCD_INTERVAL`:
/// модель с первым показанием через 30 с прятала, что медленный старт
/// индикатора — не вина датчика.
const SCD_FIRST_DATA: u64 = 5 * SEC;
const BARO_INTERVAL: u64 = 20 * SEC;
/// Время одного замера BMP390 при OSR ×32/×8 прошивки, мс: формула даташита
/// (234 + 392 + 32·2020 + 163 + 8·2020 мкс) с запасом 1.2, как у `bmp390-rs`.
const BARO_CONVERSION: u64 = 98;
/// Смещение температуры SCD41 по умолчанию (EEPROM), м°C.
const EEPROM_OFFSET: i32 = 4000;
/// Переходный процесс SCD41 после старта low-power замера (плата, 2026-10-18,
/// два старта): первое показание на ~4 °C выше установившегося, через ~3.5 мин
/// — в пределах 0.1 °C. Модель — линейный спад.
const SCD_WARMUP: u64 = 210 * SEC;
const SCD_WARMUP_C: f32 = 4.0;
/// Инвариант 1.
const MAX_CYCLE_GAP: u64 = 60 * SEC;
/// Инвариант 5: от конца залипания до первых данных.
const RECOVERY_BOUND: u64 = 4 * MIN;
/// Инвариант 6: промежуток между показаниями здорового BMP390.
const MAX_BARO_GAP: u64 = 80 * SEC;

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).expect("duration fits u64 ms")
}

fn up(t: u64) -> Uptime {
    Uptime(Duration::from_millis(t))
}

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

    /// Событие с вероятностью `permille`/1000.
    fn permille(&mut self, permille: u64) -> bool {
        self.below(1000) < permille
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[usize::try_from(self.below(items.len() as u64)).expect("index fits usize")]
    }
}

/// Случайные сбои датчика, на операцию, ‰.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Profile {
    hang:    u64,
    nack:    u64,
    crc:     u64,
    garbage: u64,
}

const CLEAN: Profile = Profile {
    hang:    0,
    nack:    0,
    crc:     0,
    garbage: 0,
};

impl Profile {
    const fn is_clean(self) -> bool {
        self.hang == 0 && self.nack == 0 && self.crc == 0 && self.garbage == 0
    }
}

/// Эпизод залипания датчика.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wedge {
    /// Всё, кроме `stop`/`reinit` (SCD41) или инициализации (BMP390), падает;
    /// переинициализация снимает залипание.
    UntilReinit,
    /// Отвечает на всё, но данных не выдаёт (застряло преобразование) до
    /// переинициализации — так датчик протухает.
    Mute,
    Until(u64),
    Forever,
}

#[derive(Debug, Clone, Copy)]
struct Episode {
    sensor: SensorId,
    from:   u64,
    wedge:  Wedge,
}

/// Модель SCD41.
struct SimScd {
    measuring: bool,
    /// Начало текущего замера (отсчёт переходного процесса).
    started:   u64,
    next_data: u64,
    data:      Option<ScdRaw>,
    offset:    i32,
    wedge:     Option<Wedge>,
    serial:    u16,
}

/// Модель BMP390.
struct SimBaro {
    initialized: bool,
    next_drdy:   u64,
    drdy:        Option<BaroRaw>,
    wedge:       Option<Wedge>,
}

#[derive(Default)]
struct Coverage {
    timeouts:     AtomicU64,
    nacks:        AtomicU64,
    crcs:         AtomicU64,
    garbage:      AtomicU64,
    scd_reinits:  AtomicU64,
    baro_reinits: AtomicU64,
    failed_inits: AtomicU64,
    stale:        AtomicU64,
    calibrations: AtomicU64,
    recoveries:   AtomicU64,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// Истина о температуре и давлении в комнате.
const ROOM_C: f32 = 22.5;
const ROOM_PA: f32 = 100_500.0;

struct World<'h> {
    rng:           Rng,
    now:           u64,
    sensors:       Sensors,
    health:        Health<'h>,
    scd:           SimScd,
    baro:          SimBaro,
    scd_prof:      Profile,
    baro_prof:     Profile,
    episodes:      Vec<Episode>,
    injected:      HashMap<(SensorId, Fault), u32>,
    /// Прочитано в текущем цикле.
    cycle_scd:     Option<ScdRaw>,
    cycle_baro:    Option<BaroRaw>,
    last_cycle:    u64,
    /// Конец последней неудачной операции SCD41 (инвариант 2, `SETTLE`).
    scd_failed_at: Option<u64>,
    /// Моменты успешных чтений.
    scd_reads:     Vec<u64>,
    baro_samples:  Vec<u64>,
    /// Закрытые циклы с моментом закрытия.
    cycles:        Vec<(u64, Cycle)>,
    set_offsets:   u32,
    cov:           &'h Coverage,
}

impl<'h> World<'h> {
    fn new(seed: u64, words: &'h [AtomicU32; WORDS], cov: &'h Coverage) -> Self {
        let (health, _) = Health::attach(words, true);
        Self {
            rng: Rng(seed),
            now: 0,
            sensors: Sensors::new(up(0)),
            health,
            // После reset MCU датчик мог продолжать замер.
            scd: SimScd {
                measuring: seed.is_multiple_of(2),
                started:   0,
                next_data: SCD_INTERVAL,
                data:      None,
                offset:    EEPROM_OFFSET,
                wedge:     None,
                serial:    0,
            },
            baro: SimBaro {
                initialized: false,
                next_drdy:   0,
                drdy:        None,
                wedge:       None,
            },
            scd_prof: CLEAN,
            baro_prof: CLEAN,
            episodes: Vec::new(),
            injected: HashMap::new(),
            cycle_scd: None,
            cycle_baro: None,
            last_cycle: 0,
            scd_failed_at: None,
            scd_reads: Vec::new(),
            baro_samples: Vec::new(),
            cycles: Vec::new(),
            set_offsets: 0,
            cov,
        }
    }

    fn inject(&mut self, sensor: SensorId, fault: Fault) {
        *self.injected.entry((sensor, fault)).or_default() += 1;
        match fault {
            Fault::Timeout => bump(&self.cov.timeouts),
            Fault::Bus => bump(&self.cov.nacks),
            Fault::Crc => bump(&self.cov.crcs),
            Fault::BadValue => bump(&self.cov.garbage),
            Fault::Rejected | Fault::Stale => {}
        }
    }

    /// Эпизоды, начавшиеся к `now`, и истёкшие залипания.
    fn apply_episodes(&mut self) {
        let now = self.now;
        for e in self.episodes.iter().filter(|e| e.from <= now) {
            let slot = match e.sensor {
                SensorId::Scd41 => &mut self.scd.wedge,
                SensorId::Bmp390 => &mut self.baro.wedge,
            };
            *slot = Some(e.wedge);
        }
        self.episodes.retain(|e| e.from > now);
        for slot in [&mut self.scd.wedge, &mut self.baro.wedge] {
            if let Some(Wedge::Until(end)) = *slot
                && end <= self.now
            {
                *slot = None;
            }
        }
    }

    fn tick_sensors(&mut self) {
        if self.scd.wedge == Some(Wedge::Mute) {
            self.scd.next_data = self.scd.next_data.max(self.now + 1);
        }
        if self.baro.wedge == Some(Wedge::Mute) {
            self.baro.next_drdy = self.baro.next_drdy.max(self.now + 1);
        }
        while self.scd.measuring && self.now >= self.scd.next_data {
            self.scd.serial = self.scd.serial.wrapping_add(1);
            let garbage = self.rng.permille(self.scd_prof.garbage);
            let offset = i16::try_from(self.scd.offset).expect("SCD41 offset is a few °C in m°C");
            let warm_left_s = (SCD_WARMUP.saturating_sub(self.now - self.scd.started)) / SEC;
            let warm_c = SCD_WARMUP_C * f32::from(u16::try_from(warm_left_s).expect("warm-up is minutes"))
                / f32::from(u16::try_from(SCD_WARMUP / SEC).expect("warm-up is minutes"));
            let temp = ROOM_C + 1.5 + warm_c - f32::from(offset) / 1000.0;
            self.scd.data = Some(ScdRaw {
                co2:      400 + self.scd.serial % 1600,
                temp_c:   if garbage { f32::NAN } else { temp },
                humidity: 45.0 + f32::from(self.scd.serial % 10),
            });
            let jitter = self.rng.below(2 * SEC + 1);
            self.scd.next_data += SCD_INTERVAL - SEC + jitter;
        }
        while self.baro.initialized && self.now >= self.baro.next_drdy {
            let garbage = self.rng.permille(self.baro_prof.garbage);
            self.baro.drdy = Some(BaroRaw {
                pressure_pa: if garbage { f32::NAN } else { ROOM_PA },
                temp_c:      ROOM_C,
            });
            self.baro.next_drdy += BARO_INTERVAL;
        }
    }

    /// Исполнить операцию: `(итог, длительность)`.
    fn exec(&mut self, op: Op) -> Result<(OpResult, u64), String> {
        self.apply_episodes();
        self.tick_sensors();
        match op.sensor() {
            SensorId::Scd41 => self.exec_scd(op),
            SensorId::Bmp390 => self.exec_baro(op),
        }
    }

    fn exec_scd(&mut self, op: Op) -> Result<(OpResult, u64), String> {
        let at = self.now / SEC;
        if let Some(failed) = self.scd_failed_at
            && self.now < failed + ms(SETTLE)
        {
            return Err(format!("{op:?} at {at}s: within SETTLE after a failed SCD41 op"));
        }
        if self.scd.measuring && !op.allowed_while_measuring() {
            return Err(format!(
                "{op:?} at {at}s: not allowed during periodic measurement"
            ));
        }
        if op == Op::ScdRead && self.scd.data.is_none() {
            return Err(format!("read at {at}s without data ready (sensor NACKs)"));
        }

        let wedged = match self.scd.wedge {
            Some(Wedge::UntilReinit) => !matches!(op, Op::ScdStop | Op::ScdReinit),
            Some(Wedge::Mute) | None => false,
            Some(Wedge::Until(_) | Wedge::Forever) => true,
        };
        let has_response = matches!(
            op,
            Op::ScdDataReady | Op::ScdRead | Op::ScdGetTempOffset | Op::ScdSerial
        );
        let p = self.scd_prof;
        let fault = if wedged {
            Some(if self.rng.permille(500) {
                Fault::Timeout
            } else {
                Fault::Bus
            })
        } else if self.rng.permille(p.hang) {
            Some(Fault::Timeout)
        } else if self.rng.permille(p.nack) {
            Some(Fault::Bus)
        } else if has_response && self.rng.permille(p.crc) {
            Some(Fault::Crc)
        } else {
            None
        };
        if let Some(fault) = fault {
            self.inject(SensorId::Scd41, fault);
            let took = match fault {
                Fault::Timeout => ms(op.timeout()),
                _ => ms(op.exec_time()) + 1,
            };
            // Ответ дошёл до датчика, но испортился в пути: буфер он уже отдал.
            if fault == Fault::Crc && op == Op::ScdRead {
                self.scd.data = None;
            }
            self.scd_failed_at = Some(self.now + took);
            return Ok((OpResult::Failed(fault), took));
        }

        let result = match op {
            Op::ScdStop => {
                self.scd.measuring = false;
                self.scd.data = None;
                OpResult::Done
            }
            Op::ScdReinit => {
                self.scd.offset = EEPROM_OFFSET;
                if matches!(self.scd.wedge, Some(Wedge::UntilReinit | Wedge::Mute)) {
                    self.scd.wedge = None;
                    bump(&self.cov.recoveries);
                }
                OpResult::Done
            }
            Op::ScdSerial | Op::ScdSetPressure(_) => OpResult::Done,
            Op::ScdGetTempOffset => OpResult::TempOffset(MilliCelsius(self.scd.offset)),
            Op::ScdSetTempOffset(offset) => {
                self.scd.offset = offset.0;
                self.set_offsets += 1;
                bump(&self.cov.calibrations);
                OpResult::Done
            }
            Op::ScdStart => {
                self.scd.measuring = true;
                self.scd.started = self.now;
                self.scd.next_data = self.now + SCD_FIRST_DATA;
                OpResult::Done
            }
            Op::ScdDataReady => OpResult::Ready(self.scd.data.is_some()),
            Op::ScdRead => {
                let raw = self.scd.data.take().expect("checked above");
                if !raw.temp_c.is_finite() {
                    self.inject(SensorId::Scd41, Fault::BadValue);
                }
                self.cycle_scd = Some(raw);
                self.scd_reads.push(self.now);
                OpResult::Scd(raw)
            }
            Op::BaroInit | Op::BaroPoll => unreachable!("BMP390 op routed to SCD41"),
        };
        Ok((result, ms(op.exec_time())))
    }

    fn exec_baro(&mut self, op: Op) -> Result<(OpResult, u64), String> {
        if op == Op::BaroPoll && !self.baro.initialized {
            return Err(format!("BMP390 polled at {}s before init", self.now / SEC));
        }
        let wedged = match self.baro.wedge {
            Some(Wedge::UntilReinit) => op != Op::BaroInit,
            Some(Wedge::Mute) | None => false,
            Some(Wedge::Until(_) | Wedge::Forever) => true,
        };
        let p = self.baro_prof;
        let fault = if wedged {
            Some(if self.rng.permille(500) {
                Fault::Timeout
            } else {
                Fault::Bus
            })
        } else if self.rng.permille(p.hang) {
            Some(Fault::Timeout)
        } else if self.rng.permille(p.nack) {
            Some(Fault::Bus)
        } else {
            None
        };
        if let Some(fault) = fault {
            self.inject(SensorId::Bmp390, fault);
            let took = if fault == Fault::Timeout {
                ms(op.timeout())
            } else {
                1
            };
            return Ok((OpResult::Failed(fault), took));
        }
        let result = match op {
            Op::BaroInit => {
                self.baro.initialized = true;
                // Конфигурация записана в конце операции.
                self.baro.next_drdy = self.now + ms(op.exec_time()) + BARO_CONVERSION;
                self.baro.drdy = None;
                if matches!(self.baro.wedge, Some(Wedge::UntilReinit | Wedge::Mute)) {
                    self.baro.wedge = None;
                    bump(&self.cov.recoveries);
                }
                OpResult::Done
            }
            Op::BaroPoll => {
                let raw = self.baro.drdy.take();
                if let Some(raw) = raw {
                    if !raw.pressure_pa.is_finite() {
                        self.inject(SensorId::Bmp390, Fault::BadValue);
                    }
                    self.cycle_baro = Some(raw);
                    self.baro_samples.push(self.now);
                }
                OpResult::Baro(raw)
            }
            _ => unreachable!("SCD41 op routed to BMP390"),
        };
        Ok((result, ms(op.exec_time())))
    }

    /// Инвариант 3: показание = прочитанное в этом цикле.
    fn check_cycle(&mut self, cycle: &Cycle) -> Result<(), String> {
        let at = self.now / SEC;
        let gap = self.now - self.last_cycle;
        if gap > MAX_CYCLE_GAP {
            return Err(format!("cycle took {}s, ended at {at}s", gap / SEC));
        }
        if gap == 0 && !self.cycles.is_empty() {
            return Err(format!("two cycles at {} ms: the automaton spins", self.now));
        }
        self.last_cycle = self.now;
        self.cycles.push((self.now, *cycle));
        let s = cycle.sample;
        let scd = self.cycle_scd.take();
        if s.co2.map(|c| c.0) != scd.map(|r| r.co2) {
            return Err(format!("co2 {:?} at {at}s, read this cycle {scd:?}", s.co2));
        }
        let want_temp = scd.and_then(|r| MilliCelsius::from_celsius(r.temp_c));
        let want_hum = scd.and_then(|r| MilliPercentRh::from_percent(r.humidity));
        if s.scd_temp != want_temp || s.humidity != want_hum {
            return Err(format!(
                "SCD41 channels {:?}/{:?} at {at}s, read {scd:?}",
                s.scd_temp, s.humidity
            ));
        }
        let baro = self.cycle_baro.take();
        let want_p =
            baro.and_then(|r| CentiPascal::from_pascal(r.pressure_pa).filter(|_| r.pressure_pa.is_finite()));
        if s.pressure != want_p {
            return Err(format!("pressure {:?} at {at}s, read {baro:?}", s.pressure));
        }
        Ok(())
    }

    fn run_until(&mut self, end: u64) -> Result<(), String> {
        while self.now < end {
            match self.sensors.next(up(self.now), &self.health) {
                Action::Run(op) => {
                    if op.timeout() > OP_TIMEOUT_MAX {
                        return Err(format!("{op:?} timeout {:?} over OP_TIMEOUT_MAX", op.timeout()));
                    }
                    let (result, took) = self.exec(op)?;
                    self.now += took;
                    self.sensors.done(result, up(self.now), &self.health);
                }
                Action::Wait(until) => {
                    let until = ms(until.0);
                    if until <= self.now {
                        return Err(format!("Wait({until}) not in the future at {}", self.now));
                    }
                    self.now = until;
                }
                Action::Cycle(cycle) => self.check_cycle(&cycle)?,
            }
        }
        Ok(())
    }

    fn counter(&self, word: Word) -> u32 {
        self.health.get(word)
    }

    /// Инвариант 4.
    fn check_accounting(&self) -> Result<(), String> {
        for sensor in [SensorId::Scd41, SensorId::Bmp390] {
            for fault in [Fault::Timeout, Fault::Bus, Fault::Crc, Fault::BadValue] {
                let injected = self.injected.get(&(sensor, fault)).copied().unwrap_or(0);
                let counted = self.counter(fault_counter(sensor, fault));
                if injected != counted {
                    return Err(format!(
                        "{sensor:?} {fault:?}: injected {injected}, journal {counted}"
                    ));
                }
            }
            let rejected = self.counter(fault_counter(sensor, Fault::Rejected));
            if rejected != 0 {
                return Err(format!("{sensor:?}: {rejected} rejected commands"));
            }
        }
        Ok(())
    }

    /// Инвариант 5, вторая часть: здоровый датчик чист в журнале.
    fn check_healthy(&self, sensor: SensorId) -> Result<(), String> {
        for fault in Fault::ALL {
            let n = self.counter(fault_counter(sensor, fault));
            if n != 0 {
                return Err(format!("healthy {sensor:?}: {n} × {fault:?}"));
            }
        }
        let (ok, failed) = reinit_counters(sensor);
        if self.counter(ok) + self.counter(failed) != 0 {
            return Err(format!("healthy {sensor:?} was reinitialised"));
        }
        Ok(())
    }

    /// Инвариант 5, первая часть: данные вернулись после залипания.
    fn check_recovery(&self, sensor: SensorId, wedge_end: u64, run_end: u64) -> Result<(), String> {
        if wedge_end + RECOVERY_BOUND > run_end {
            return Ok(());
        }
        let reads = match sensor {
            SensorId::Scd41 => &self.scd_reads,
            SensorId::Bmp390 => &self.baro_samples,
        };
        let back = reads.iter().find(|t| **t >= wedge_end);
        match back {
            Some(t) if *t <= wedge_end + RECOVERY_BOUND => Ok(()),
            _ => {
                Err(format!(
                    "{sensor:?} data not back {}s after the wedge ended at {}s",
                    RECOVERY_BOUND / SEC,
                    wedge_end / SEC
                ))
            }
        }
    }

    /// Промежуток между показаниями BMP390 (инвариант 6).
    fn max_baro_gap(&self, from: u64) -> u64 {
        let mut prev = from;
        let mut gap = 0;
        for t in self.baro_samples.iter().filter(|t| **t >= from) {
            gap = gap.max(t - prev);
            prev = *t;
        }
        gap.max(self.now - prev)
    }
}

const PROFILES: [Profile; 4] = [
    CLEAN,
    Profile {
        hang:    5,
        nack:    5,
        crc:     5,
        garbage: 10,
    },
    Profile {
        hang:    50,
        nack:    30,
        crc:     30,
        garbage: 50,
    },
    Profile {
        hang:    300,
        nack:    200,
        crc:     100,
        garbage: 100,
    },
];

fn run(seed: u64, cov: &Coverage) -> Result<(), String> {
    let words: [AtomicU32; WORDS] = core::array::from_fn(|_| AtomicU32::new(0));
    let mut world = World::new(seed, &words, cov);
    let run_end = 4 * HOUR;
    world.scd_prof = world.rng.pick(&PROFILES);
    world.baro_prof = world.rng.pick(&PROFILES);

    let mut ends = Vec::new();
    for _ in 0..world.rng.below(4) {
        let sensor = world.rng.pick(&[SensorId::Scd41, SensorId::Bmp390]);
        let from = world.rng.below(run_end);
        let wedge = match world.rng.below(4) {
            0 => Wedge::UntilReinit,
            1 => Wedge::Mute,
            2 => Wedge::Until(from + SEC + world.rng.below(20 * MIN)),
            _ => Wedge::Forever,
        };
        match wedge {
            Wedge::UntilReinit | Wedge::Mute => ends.push((sensor, from)),
            Wedge::Until(end) => ends.push((sensor, end)),
            Wedge::Forever => {}
        }
        world.episodes.push(Episode { sensor, from, wedge });
    }
    let episodes = world.episodes.clone();
    world.run_until(run_end)?;
    world.check_accounting()?;

    for sensor in [SensorId::Scd41, SensorId::Bmp390] {
        let prof = match sensor {
            SensorId::Scd41 => world.scd_prof,
            SensorId::Bmp390 => world.baro_prof,
        };
        let wedged = episodes.iter().any(|e| e.sensor == sensor);
        if prof.is_clean() && !wedged {
            world.check_healthy(sensor)?;
        }
        if prof.is_clean() {
            // Эпизоды друг за другом мешают оценке — проверяем одиночные.
            if episodes.iter().filter(|e| e.sensor == sensor).count() == 1 {
                for (s, end) in ends.iter().filter(|(s, _)| *s == sensor) {
                    world.check_recovery(*s, *end, run_end)?;
                }
            }
        }
    }
    let stale = world.counter(Word::Scd41Stale) + world.counter(Word::Bmp390Stale);
    cov.stale.fetch_add(u64::from(stale), Ordering::Relaxed);
    let (scd_ok, scd_failed) = reinit_counters(SensorId::Scd41);
    let (baro_ok, baro_failed) = reinit_counters(SensorId::Bmp390);
    cov.scd_reinits
        .fetch_add(u64::from(world.counter(scd_ok)), Ordering::Relaxed);
    cov.baro_reinits
        .fetch_add(u64::from(world.counter(baro_ok)), Ordering::Relaxed);
    cov.failed_inits.fetch_add(
        u64::from(world.counter(scd_failed) + world.counter(baro_failed)),
        Ordering::Relaxed,
    );

    // Инвариант 6: живой BMP390 не ждёт мёртвый SCD41.
    let scd_dead = episodes
        .iter()
        .any(|e| e.sensor == SensorId::Scd41 && e.wedge == Wedge::Forever);
    let baro_clean = world.baro_prof.is_clean() && !episodes.iter().any(|e| e.sensor == SensorId::Bmp390);
    if scd_dead && baro_clean {
        let gap = world.max_baro_gap(MIN);
        if gap > MAX_BARO_GAP {
            return Err(format!("BMP390 gap {}s while SCD41 is dead", gap / SEC));
        }
    }
    Ok(())
}

#[test]
fn sensors_survive_faults() {
    let mut runner = TestRunner::new(Config {
        cases: 256,
        rng_seed: RngSeed::Fixed(0x6d65_7465_6f5f_7334),
        failure_persistence: None,
        ..Config::default()
    });
    let cov = Coverage::default();
    runner
        .run(&any::<u64>(), |seed| run(seed, &cov).map_err(TestCaseError::fail))
        .unwrap();

    let scenarios = [
        ("timeouts", &cov.timeouts),
        ("NACKs", &cov.nacks),
        ("CRC errors", &cov.crcs),
        ("garbage values", &cov.garbage),
        ("SCD41 reinits", &cov.scd_reinits),
        ("BMP390 reinits", &cov.baro_reinits),
        ("failed inits", &cov.failed_inits),
        ("stale sensors", &cov.stale),
        ("temperature calibrations", &cov.calibrations),
        ("wedges cleared by reinit", &cov.recoveries),
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

/// Без сбоев: каждый цикл несёт CO2, калибровка ровно одна и сводит
/// температуру SCD41 к BMP390, журнал чист.
#[test]
fn healthy_sensors_give_every_channel() {
    let cov = Coverage::default();
    let words: [AtomicU32; WORDS] = core::array::from_fn(|_| AtomicU32::new(0));
    let mut world = World::new(1, &words, &cov);
    world.run_until(2 * HOUR).unwrap();
    world.check_healthy(SensorId::Scd41).unwrap();
    world.check_healthy(SensorId::Bmp390).unwrap();
    assert_eq!(
        world.set_offsets, 1,
        "temperature offset calibrated once per boot"
    );
    // Сырой SCD41 на 1.5 °C теплее комнаты ⇒ смещение 1.5 °C.
    assert_eq!(world.scd.offset, 1500);
    let reads = world.scd_reads.len();
    assert!(reads >= 230, "only {reads} SCD41 reads in 2 h");
    assert!(world.max_baro_gap(MIN) <= MAX_BARO_GAP);
}

/// Старт: первый цикл несёт все каналы. Опрос BMP390 сразу за инициализацией
/// застал бы `drdy` пустым, и давление пришло бы только через период ODR,
/// циклом позже. Оба исхода прошлой жизни MCU: SCD41 замерял или стоял.
#[test]
fn first_cycle_carries_pressure() {
    for seed in [0, 1] {
        let cov = Coverage::default();
        let words: [AtomicU32; WORDS] = core::array::from_fn(|_| AtomicU32::new(0));
        let mut world = World::new(seed, &words, &cov);
        world.run_until(2 * MIN).unwrap();
        let (at, first) = world.cycles[0];
        assert!(
            first.sample.pressure.is_some() && first.sample.co2.is_some() && first.periph_ok,
            "seed {seed}: first cycle at {at} ms: {first:?}"
        );
        world.check_healthy(SensorId::Scd41).unwrap();
        world.check_healthy(SensorId::Bmp390).unwrap();
    }
}

/// Старт: индикатор получает CO2 через секунды — автомат не задерживает
/// первое показание датчика. Оба исхода прошлой жизни MCU: SCD41 замерял или
/// стоял.
#[test]
fn first_co2_within_seconds() {
    const FIRST_CO2_BOUND: u64 = 10 * SEC;
    for seed in [0, 1] {
        let cov = Coverage::default();
        let words: [AtomicU32; WORDS] = core::array::from_fn(|_| AtomicU32::new(0));
        let mut world = World::new(seed, &words, &cov);
        world.run_until(2 * MIN).unwrap();
        let (at, first) = world.cycles[0];
        assert!(
            first.sample.co2.is_some() && at <= FIRST_CO2_BOUND,
            "seed {seed}: first cycle at {at} ms: {first:?}"
        );
        world.check_healthy(SensorId::Scd41).unwrap();
    }
}

/// Залипший до `reinit` SCD41 оживает переинициализацией без перезагрузки.
#[test]
fn wedged_scd41_recovers_by_reinit() {
    let cov = Coverage::default();
    let words: [AtomicU32; WORDS] = core::array::from_fn(|_| AtomicU32::new(0));
    let mut world = World::new(2, &words, &cov);
    world.run_until(10 * MIN).unwrap();
    world.episodes.push(Episode {
        sensor: SensorId::Scd41,
        from:   world.now,
        wedge:  Wedge::UntilReinit,
    });
    let wedged_at = world.now;
    world.run_until(wedged_at + 30 * MIN).unwrap();
    world.check_accounting().unwrap();
    assert_eq!(world.counter(Word::Scd41Reinit), 1, "one reinit clears the wedge");
    world
        .check_recovery(SensorId::Scd41, wedged_at, world.now)
        .unwrap();
    assert!(world.max_baro_gap(MIN) <= MAX_BARO_GAP, "BMP390 kept going");
}
