//! Опрос датчиков (SCD41 по I2C, BMP390 по SPI) как sans-IO автомат.
//!
//! Время явное, как у `sender`: автомат решает, какую операцию с шиной сделать
//! ([`Sensors::next`]), драйвер прошивки исполняет её и возвращает итог
//! ([`Sensors::done`]). Шины и часов автомат не трогает ⇒ DST гоняет его против
//! модели датчиков на виртуальном времени (`tests/sensor_dst.rs`).
//!
//! Предел: у каждой операции свой [`Op::timeout`] (время исполнения команды по
//! даташиту + [`BUS_SLACK`]). Драйвер обязан исполнять КАЖДУЮ операцию под этим
//! пределом и возвращать [`Fault::Timeout`] по его истечении — тогда цикл
//! завершается не позже [`CYCLE_MAX`] + хвост операций, что бы ни делала шина.
//!
//! SCD41 — в low-power periodic (данные раз в ~30 с): ASC считает время
//! в часах работы, а не числом single-shot замеров с интервалом 5 мин, и
//! 5-секундного ожидания single-shot на цикл нет. Исключение — первое
//! показание после boot: первые данные low-power приходят только через ~30 с,
//! и всё это время индикатору нечего показывать. Поэтому стартовая настройка
//! запускает обычный periodic (данные через ~5 с), а после первого чтения
//! датчик переводится в low-power (stop → start, [`ScdCadence`]). Во время
//! замера датчику шлём только разрешённое даташитом (табл. 9):
//! `read_measurement`, `get_data_ready_status`, `set_ambient_pressure`,
//! `stop_periodic_measurement`.
//!
//! Цикл: SCD41 (опрос готовности раз в [`SCD_POLL`], чтение) → BMP390 →
//! компенсация давления в SCD41 → разовая калибровка смещения температуры
//! (не раньше [`SCD_WARMUP`] от старта замера) → [`Action::Cycle`]. Сбой ⇒ журнал; [`REINIT_AFTER`] сбоев подряд ⇒
//! переинициализация датчика; неудачная ⇒ повтор через [`REINIT_BACKOFF`].
//! Отказ одного датчика не прерывает опрос другого.

use core::time::Duration;

use crate::codec::{
    CentiPascal,
    EpochMillis,
    MilliCelsius,
    MilliPercentRh,
    Ppm,
    Reading,
};
use crate::health::Health;
use crate::supervisor::Uptime;

/// Запас сверх времени исполнения команды на саму передачу по шине.
pub const BUS_SLACK: Duration = Duration::from_secs(1);
/// Наибольший предел одной операции (проверяется тестом по всем `Op`).
pub const OP_TIMEOUT_MAX: Duration = Duration::from_millis(1500);
/// Как часто спрашивать SCD41 о готовности данных.
pub const SCD_POLL: Duration = Duration::from_secs(2);
/// Дольше этого цикл не ждёт данных SCD41 (low-power periodic — ~30 с).
pub const CYCLE_MAX: Duration = Duration::from_secs(45);
/// Темп циклов, когда ждать SCD41 нечего (датчик на паузе восстановления).
pub const CYCLE_PERIOD: Duration = Duration::from_secs(30);
/// Нет данных SCD41 столько ⇒ сбой `Stale` (два пропущенных интервала).
pub const SCD_STALE: Duration = Duration::from_secs(75);
/// Нет данных BMP390 столько ⇒ сбой `Stale` (ODR 0.05 Гц — раз в 20 с).
pub const BARO_STALE: Duration = Duration::from_secs(60);
/// Пауза от инициализации BMP390 до первого опроса.
///
/// Normal-режим, включённый переходом из Sleep (так делает драйвер прошивки),
/// начинается с преобразования; при OSR ×32/×8 прошивки оно идёт до 98 мс
/// (формула даташита с запасом 1.2). Опрос раньше застал бы `drdy` пустым,
/// а следующее преобразование — только через период ODR (20 с).
pub const BARO_CONVERSION: Duration = Duration::from_millis(100);
/// Пауза после сбоя операции перед следующей командой тому же датчику:
/// снятая по таймауту команда могла ещё исполняться.
pub const SETTLE: Duration = Duration::from_secs(1);
/// Столько сбоев подряд ⇒ переинициализация датчика.
pub const REINIT_AFTER: u32 = 3;
/// Пауза после неудачной инициализации перед следующей попыткой.
pub const REINIT_BACKOFF: Duration = Duration::from_secs(60);
/// Замер SCD41 идёт столько до калибровки смещения температуры.
///
/// После старта low-power замера температура ~3.5 мин
/// сходит на ~4 °C (плата, 2026-10-18): калибровка по первому показанию
/// завышала смещение на эти градусы, а влажность — на ~17 п.п.
pub const SCD_WARMUP: Duration = Duration::from_secs(300);

/// Диапазон компенсации давления SCD41, гПа (даташит: 700–1200 гПа).
const SCD_PRESSURE_HPA: core::ops::RangeInclusive<f32> = 700.0..=1200.0;
/// Рабочий диапазон BMP390, Па (300–1250 гПа).
const BARO_PRESSURE_PA: core::ops::RangeInclusive<f32> = 30_000.0..=125_000.0;
/// Рабочий диапазон температуры обоих датчиков, °C.
const TEMP_C: core::ops::RangeInclusive<f32> = -40.0..=85.0;
/// Влажность — проценты.
const HUMIDITY_PCT: core::ops::RangeInclusive<f32> = 0.0..=100.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SensorId {
    Scd41,
    Bmp390,
}

/// Код операции для журнала (этап таски, последний сбой). 0 — ожидание.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpCode(pub u32);

impl OpCode {
    /// Таска ждёт таймер, а не шину.
    pub const WAIT: Self = Self(0);

    /// Имя для журнала приёмника.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self.0 {
            0 => "wait",
            1 => "scd41.stop",
            2 => "scd41.reinit",
            3 => "scd41.serial",
            4 => "scd41.get_temp_offset",
            5 => "scd41.set_temp_offset",
            6 => "scd41.start",
            7 => "scd41.data_ready",
            8 => "scd41.read",
            9 => "scd41.set_pressure",
            21 => "bmp390.init",
            22 => "bmp390.poll",
            _ => "unknown",
        }
    }
}

/// Вид сбоя операции с датчиком.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fault {
    /// Операция не уложилась в [`Op::timeout`] и снята.
    Timeout,
    /// Ошибка шины: NACK, потеря арбитража, аппаратный таймаут, ошибка SPI.
    Bus,
    /// Ответ не прошёл CRC.
    Crc,
    /// Драйвер отказался исполнять команду (состояние, параметр).
    Rejected,
    /// Датчик отвечает, но данных нет дольше допустимого.
    Stale,
    /// Значение вне рабочего диапазона датчика или не число.
    BadValue,
}

impl Fault {
    pub const ALL: [Self; 6] = [
        Self::Timeout,
        Self::Bus,
        Self::Crc,
        Self::Rejected,
        Self::Stale,
        Self::BadValue,
    ];

    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Timeout => 1,
            Self::Bus => 2,
            Self::Crc => 3,
            Self::Rejected => 4,
            Self::Stale => 5,
            Self::BadValue => 6,
        }
    }

    /// Имя по коду из журнала.
    #[must_use]
    pub const fn name_of(code: u32) -> &'static str {
        match code {
            0 => "none",
            1 => "timeout",
            2 => "bus",
            3 => "crc",
            4 => "rejected",
            5 => "stale",
            6 => "bad_value",
            _ => "unknown",
        }
    }
}

/// Давление для компенсации SCD41, целые гПа.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hectopascal(pub u16);

/// Режим periodic-замера SCD41.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScdCadence {
    /// `start_periodic_measurement`: данные раз в 5 с. Только ради первого
    /// показания после boot — дальше датчик переводится в [`Self::LowPower`].
    Fast,
    /// `start_low_power_periodic_measurement`: данные раз в ~30 с.
    LowPower,
}

/// Операция с шиной.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    ScdStop,
    ScdReinit,
    ScdSerial,
    ScdGetTempOffset,
    ScdSetTempOffset(MilliCelsius),
    ScdStart(ScdCadence),
    ScdDataReady,
    ScdRead,
    ScdSetPressure(Hectopascal),
    /// (Пере)создать драйвер BMP390: проверка id, запись конфигурации.
    BaroInit,
    /// Статус `drdy` и, если готово, данные.
    BaroPoll,
}

impl Op {
    #[must_use]
    pub const fn sensor(self) -> SensorId {
        match self {
            Self::BaroInit | Self::BaroPoll => SensorId::Bmp390,
            _ => SensorId::Scd41,
        }
    }

    #[must_use]
    pub const fn code(self) -> OpCode {
        OpCode(match self {
            Self::ScdStop => 1,
            Self::ScdReinit => 2,
            Self::ScdSerial => 3,
            Self::ScdGetTempOffset => 4,
            Self::ScdSetTempOffset(_) => 5,
            Self::ScdStart(_) => 6,
            Self::ScdDataReady => 7,
            Self::ScdRead => 8,
            Self::ScdSetPressure(_) => 9,
            Self::BaroInit => 21,
            Self::BaroPoll => 22,
        })
    }

    /// Время исполнения по даташиту (у SCD41 драйвер выжидает его сам после
    /// записи команды; у BMP390 — пауза драйвера на старте и чтение регистров).
    #[must_use]
    pub const fn exec_time(self) -> Duration {
        match self {
            Self::ScdStop => Duration::from_millis(500),
            Self::ScdReinit => Duration::from_millis(20),
            Self::ScdStart(_) => Duration::ZERO,
            Self::ScdSerial
            | Self::ScdGetTempOffset
            | Self::ScdSetTempOffset(_)
            | Self::ScdDataReady
            | Self::ScdRead
            | Self::ScdSetPressure(_) => Duration::from_millis(1),
            Self::BaroInit => Duration::from_millis(400),
            Self::BaroPoll => Duration::from_millis(10),
        }
    }

    /// Предел операции для драйвера.
    #[must_use]
    pub const fn timeout(self) -> Duration {
        self.exec_time().saturating_add(BUS_SLACK)
    }

    /// Разрешена ли команда SCD41 во время periodic-замера (даташит, табл. 9).
    #[must_use]
    pub const fn allowed_while_measuring(self) -> bool {
        matches!(
            self,
            Self::ScdRead | Self::ScdDataReady | Self::ScdSetPressure(_) | Self::ScdStop
        )
    }
}

/// Сырое показание SCD41, как его отдаёт драйвер.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScdRaw {
    pub co2:      u16,
    pub temp_c:   f32,
    pub humidity: f32,
}

/// Сырое показание BMP390.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BaroRaw {
    pub pressure_pa: f32,
    pub temp_c:      f32,
}

/// Итог операции.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OpResult {
    /// Команда без ответа исполнена.
    Done,
    TempOffset(MilliCelsius),
    Ready(bool),
    Scd(ScdRaw),
    /// `None` — `drdy` не стоит, новых данных нет (штатно).
    Baro(Option<BaroRaw>),
    Failed(Fault),
}

/// Значения одного цикла. Канал есть, только если прочитан в этом цикле и
/// в рабочем диапазоне.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sample {
    pub pressure:  Option<CentiPascal>,
    pub baro_temp: Option<MilliCelsius>,
    pub co2:       Option<Ppm>,
    pub humidity:  Option<MilliPercentRh>,
    pub scd_temp:  Option<MilliCelsius>,
}

impl Sample {
    /// Хотя бы один канал прочитан. Пустое показание на сервер не идёт.
    #[must_use]
    pub const fn has_any_channel(&self) -> bool {
        self.pressure.is_some()
            || self.baro_temp.is_some()
            || self.co2.is_some()
            || self.humidity.is_some()
            || self.scd_temp.is_some()
    }

    #[must_use]
    pub const fn into_reading(self, time: EpochMillis) -> Reading {
        Reading {
            time,
            pressure: self.pressure,
            baro_temp: self.baro_temp,
            co2: self.co2,
            humidity: self.humidity,
            scd_temp: self.scd_temp,
        }
    }
}

/// Итог цикла.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cycle {
    pub sample:    Sample,
    /// Оба датчика в строю: SCD41 дал данные, BMP390 без сбоя и не протух.
    pub periph_ok: bool,
}

/// Что делать драйверу.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Исполнить операцию под [`Op::timeout`] и вернуть итог в [`Sensors::done`].
    Run(Op),
    /// Спать до этого момента и снова спросить [`Sensors::next`].
    Wait(Uptime),
    /// Цикл завершён (heartbeat sensor-таски).
    Cycle(Cycle),
}

/// Режим SCD41, как его знает автомат.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScdMode {
    /// После boot или сбоя `stop`/`start`: мог и замерять.
    Unknown,
    Idle,
    Measuring(ScdCadence),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScdPhase {
    /// Последовательность настройки; `recovery` — повторная (считается
    /// переинициализацией).
    Setup {
        recovery: bool,
        step:     usize,
    },
    Measuring,
    /// Первое показание взято в [`ScdCadence::Fast`]: перевод в low-power,
    /// stop → start.
    SlowDown {
        step: usize,
    },
    /// Разовая калибровка смещения температуры: stop → set → start.
    Calibrate {
        offset: MilliCelsius,
        step:   usize,
    },
    /// После неудачной настройки — ждать до `until`.
    Backoff {
        until: Uptime,
    },
}

/// Первая настройка: остановить возможный замер прошлой жизни MCU, проверить
/// датчик, узнать смещение, запустить замер. Замер — быстрый: первое показание
/// через ~5 с, а не через ~30 с low-power (см. [`ScdPhase::SlowDown`]).
const SETUP: [Op; 4] = [
    Op::ScdStop,
    Op::ScdSerial,
    Op::ScdGetTempOffset,
    Op::ScdStart(ScdCadence::Fast),
];
/// Переинициализация: `reinit` разрешён только после `stop` (даташит 3.9.5) и
/// перечитывает настройки из EEPROM ⇒ смещение снова узнаём. Старт быстрый,
/// как у первой настройки: после восстановления данные нужны поскорее.
const REINIT: [Op; 4] = [
    Op::ScdStop,
    Op::ScdReinit,
    Op::ScdGetTempOffset,
    Op::ScdStart(ScdCadence::Fast),
];

#[derive(Debug, Clone, Copy)]
struct Scd {
    mode:       ScdMode,
    phase:      ScdPhase,
    failures:   u32,
    /// Раньше этого момента команду не слать (пауза опроса или после сбоя).
    not_before: Uptime,
    /// Последние данные или старт замера: отсчёт `SCD_STALE`.
    last_data:  Uptime,
    /// Старт текущего замера: отсчёт `SCD_WARMUP` до калибровки.
    started:    Uptime,
    /// `get_data_ready_status` ответил «готово»: следующая — чтение.
    ready:      bool,
    /// Смещение температуры в датчике сейчас.
    offset:     Option<MilliCelsius>,
    calibrated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaroPhase {
    NeedsInit { recovery: bool },
    Ready,
    Backoff { until: Uptime },
}

#[derive(Debug, Clone, Copy)]
struct Baro {
    phase:      BaroPhase,
    failures:   u32,
    last_data:  Uptime,
    /// Раньше этого момента не опрашивать: идёт первое преобразование.
    not_before: Uptime,
}

/// Накопитель текущего цикла.
#[derive(Debug, Clone, Copy)]
struct CycleState {
    start:         Uptime,
    sample:        Sample,
    /// Сырая температура SCD41 этого цикла — для калибровки смещения.
    scd_temp_raw:  Option<f32>,
    baro_temp_raw: Option<f32>,
    pressure_hpa:  Option<Hectopascal>,
    scd:           Turn,
    baro:          Turn,
    pressure_sent: bool,
}

/// Очередь датчика в текущем цикле.
#[derive(Debug, Clone, Copy, Default)]
struct Turn {
    /// В этом цикле датчику больше нечего делать.
    done:   bool,
    /// В этом цикле была неудачная операция.
    failed: bool,
}

impl CycleState {
    const fn new(start: Uptime) -> Self {
        Self {
            start,
            sample: Sample {
                pressure:  None,
                baro_temp: None,
                co2:       None,
                humidity:  None,
                scd_temp:  None,
            },
            scd_temp_raw: None,
            baro_temp_raw: None,
            pressure_hpa: None,
            scd: Turn {
                done:   false,
                failed: false,
            },
            baro: Turn {
                done:   false,
                failed: false,
            },
            pressure_sent: false,
        }
    }
}

pub struct Sensors {
    scd:     Scd,
    baro:    Baro,
    cycle:   CycleState,
    running: Option<Op>,
}

const fn after(t: Uptime, d: Duration) -> Uptime {
    Uptime(t.0.saturating_add(d))
}

impl Sensors {
    #[must_use]
    pub const fn new(now: Uptime) -> Self {
        Self {
            scd:     Scd {
                mode:       ScdMode::Unknown,
                phase:      ScdPhase::Setup {
                    recovery: false,
                    step:     0,
                },
                failures:   0,
                not_before: now,
                last_data:  now,
                started:    now,
                ready:      false,
                offset:     None,
                calibrated: false,
            },
            baro:    Baro {
                phase:      BaroPhase::NeedsInit { recovery: false },
                failures:   0,
                last_data:  now,
                not_before: now,
            },
            cycle:   CycleState::new(now),
            running: None,
        }
    }

    /// Следующее действие. Журнал — для сбоев, обнаруженных на закрытии цикла
    /// (датчик протух).
    ///
    /// # Panics
    ///
    /// Паникует, если предыдущая операция не завершена через [`Sensors::done`].
    pub fn next(&mut self, now: Uptime, health: &Health<'_>) -> Action {
        assert!(
            self.running.is_none(),
            "next() before done() of {:?}",
            self.running
        );
        let action = self.decide(now, health);
        if let Action::Run(op) = action {
            self.running = Some(op);
        }
        action
    }

    fn decide(&mut self, now: Uptime, health: &Health<'_>) -> Action {
        if !self.cycle.scd.done
            && let Some(action) = self.scd_step(now)
        {
            return action;
        }
        if !self.cycle.baro.done
            && let Some(action) = self.baro_step(now)
        {
            return action;
        }
        if let Some(op) = self.pressure_step() {
            return Action::Run(op);
        }
        if let Some(action) = self.restart_step(now) {
            return action;
        }
        // Ждать SCD41 было нечего (пауза восстановления) ⇒ держим темп циклов.
        let paced = after(self.cycle.start, CYCLE_PERIOD);
        if self.cycle.sample.co2.is_none() && now < paced && !matches!(self.scd.phase, ScdPhase::Measuring) {
            return Action::Wait(paced);
        }
        Action::Cycle(self.finish(now, health))
    }

    /// Шаг SCD41 текущего цикла; `None` — SCD41 в этом цикле закончил.
    fn scd_step(&mut self, now: Uptime) -> Option<Action> {
        loop {
            match self.scd.phase {
                ScdPhase::Backoff { until } => {
                    if now < until {
                        self.cycle.scd.done = true;
                        return None;
                    }
                    self.scd.phase = ScdPhase::Setup {
                        recovery: true,
                        step:     0,
                    };
                }
                ScdPhase::Setup { recovery, step } => {
                    if now < self.scd.not_before {
                        return Some(Action::Wait(self.scd.not_before));
                    }
                    let seq = if recovery { REINIT } else { SETUP };
                    return Some(Action::Run(seq[step]));
                }
                ScdPhase::Measuring => {
                    if self.scd.ready {
                        return Some(Action::Run(Op::ScdRead));
                    }
                    let deadline = after(self.cycle.start, CYCLE_MAX);
                    if now >= deadline {
                        self.cycle.scd.done = true;
                        return None;
                    }
                    if now < self.scd.not_before {
                        return Some(Action::Wait(self.scd.not_before.min(deadline)));
                    }
                    return Some(Action::Run(Op::ScdDataReady));
                }
                // Перезапуск замера — после опроса обоих датчиков, не здесь.
                ScdPhase::SlowDown { .. } | ScdPhase::Calibrate { .. } => {
                    self.cycle.scd.done = true;
                    return None;
                }
            }
        }
    }

    fn baro_step(&mut self, now: Uptime) -> Option<Action> {
        match self.baro.phase {
            BaroPhase::Backoff { until } if now < until => {
                self.cycle.baro.done = true;
                None
            }
            BaroPhase::Backoff { .. } => {
                self.baro.phase = BaroPhase::NeedsInit { recovery: true };
                Some(Action::Run(Op::BaroInit))
            }
            BaroPhase::NeedsInit { .. } => Some(Action::Run(Op::BaroInit)),
            BaroPhase::Ready if now < self.baro.not_before => Some(Action::Wait(self.baro.not_before)),
            BaroPhase::Ready => Some(Action::Run(Op::BaroPoll)),
        }
    }

    fn pressure_step(&mut self) -> Option<Op> {
        let c = &mut self.cycle;
        if c.pressure_sent || c.scd.failed || !matches!(self.scd.mode, ScdMode::Measuring(_)) {
            return None;
        }
        let hpa = c.pressure_hpa?;
        c.pressure_sent = true;
        Some(Op::ScdSetPressure(hpa))
    }

    /// Перезапуск замера SCD41 в хвосте цикла: перевод в low-power или
    /// калибровка смещения.
    fn restart_step(&mut self, now: Uptime) -> Option<Action> {
        match self.scd.phase {
            ScdPhase::SlowDown { step } => {
                if now < self.scd.not_before {
                    return Some(Action::Wait(self.scd.not_before));
                }
                Some(Action::Run(match step {
                    0 => Op::ScdStop,
                    _ => Op::ScdStart(ScdCadence::LowPower),
                }))
            }
            ScdPhase::Calibrate { offset, step } => {
                if now < self.scd.not_before {
                    return Some(Action::Wait(self.scd.not_before));
                }
                Some(Action::Run(match step {
                    0 => Op::ScdStop,
                    1 => Op::ScdSetTempOffset(offset),
                    _ => Op::ScdStart(ScdCadence::LowPower),
                }))
            }
            ScdPhase::Measuring
                if !self.scd.calibrated
                    && !self.cycle.scd.failed
                    && now >= after(self.scd.started, SCD_WARMUP) =>
            {
                let offset = calibrated_offset(
                    self.cycle.scd_temp_raw?,
                    self.cycle.baro_temp_raw?,
                    self.scd.offset?,
                )?;
                self.scd.phase = ScdPhase::Calibrate { offset, step: 0 };
                Some(Action::Run(Op::ScdStop))
            }
            _ => None,
        }
    }

    /// Итог операции из последнего [`Action::Run`].
    ///
    /// # Panics
    ///
    /// Паникует, если операции не было или итог не того вида (дефект драйвера).
    pub fn done(&mut self, result: OpResult, now: Uptime, health: &Health<'_>) {
        let op = self.running.take().expect("done() without a running op");
        if let OpResult::Failed(fault) = result {
            health.sensor_fault(op.sensor(), op.code(), fault, now);
            match op.sensor() {
                SensorId::Scd41 => self.scd_failed(op, now, health),
                SensorId::Bmp390 => self.baro_failed(op, now, health),
            }
            return;
        }
        match (op, result) {
            (Op::ScdStop, OpResult::Done) => {
                self.scd.mode = ScdMode::Idle;
                self.scd.ready = false;
                self.scd_advance(now, health);
            }
            (Op::ScdReinit, OpResult::Done) => {
                self.scd.offset = None;
                self.scd.calibrated = false;
                self.scd_advance(now, health);
            }
            (Op::ScdSerial, OpResult::Done) => self.scd_advance(now, health),
            (Op::ScdGetTempOffset, OpResult::TempOffset(offset))
            | (Op::ScdSetTempOffset(offset), OpResult::Done) => {
                self.scd.offset = Some(offset);
                self.scd_advance(now, health);
            }
            (Op::ScdStart(cadence), OpResult::Done) => {
                self.scd.mode = ScdMode::Measuring(cadence);
                self.scd.last_data = now;
                self.scd.started = now;
                self.scd.ready = false;
                self.scd.not_before = after(now, SCD_POLL);
                self.scd_advance(now, health);
            }
            (Op::ScdDataReady, OpResult::Ready(ready)) => {
                self.scd.ready = ready;
                if !ready {
                    self.scd.not_before = after(now, SCD_POLL);
                }
            }
            (Op::ScdRead, OpResult::Scd(raw)) => {
                self.scd.ready = false;
                self.scd.failures = 0;
                self.scd.last_data = now;
                self.scd.not_before = after(now, SCD_POLL);
                self.cycle.scd.done = true;
                self.take_scd(raw, now, health);
                if self.scd.mode == ScdMode::Measuring(ScdCadence::Fast) {
                    self.scd.phase = ScdPhase::SlowDown { step: 0 };
                }
            }
            (Op::ScdSetPressure(_), OpResult::Done) => {}
            (Op::BaroInit, OpResult::Done) => {
                if self.baro.phase == (BaroPhase::NeedsInit { recovery: true }) {
                    health.sensor_reinit(SensorId::Bmp390, true);
                }
                self.baro.phase = BaroPhase::Ready;
                self.baro.failures = 0;
                self.baro.last_data = now;
                self.baro.not_before = after(now, BARO_CONVERSION);
            }
            (Op::BaroPoll, OpResult::Baro(raw)) => {
                self.cycle.baro.done = true;
                if let Some(raw) = raw {
                    self.baro.failures = 0;
                    self.baro.last_data = now;
                    self.take_baro(raw, now, health);
                }
            }
            (op, result) => panic!("{op:?} cannot end with {result:?}"),
        }
    }

    /// Успешный шаг последовательности SCD41.
    fn scd_advance(&mut self, now: Uptime, health: &Health<'_>) {
        match self.scd.phase {
            ScdPhase::Setup { recovery, step } => {
                if step + 1 < SETUP.len() {
                    self.scd.phase = ScdPhase::Setup {
                        recovery,
                        step: step + 1,
                    };
                } else {
                    if recovery {
                        health.sensor_reinit(SensorId::Scd41, true);
                    }
                    self.scd.failures = 0;
                    self.scd.phase = ScdPhase::Measuring;
                }
            }
            ScdPhase::SlowDown { step } => {
                if step < 1 {
                    self.scd.phase = ScdPhase::SlowDown { step: step + 1 };
                } else {
                    self.scd.phase = ScdPhase::Measuring;
                }
            }
            ScdPhase::Calibrate { offset, step } => {
                if step < 2 {
                    self.scd.phase = ScdPhase::Calibrate {
                        offset,
                        step: step + 1,
                    };
                } else {
                    self.scd.calibrated = true;
                    self.scd.phase = ScdPhase::Measuring;
                }
            }
            ScdPhase::Measuring | ScdPhase::Backoff { .. } => {
                unreachable!("SCD41 sequence step outside a sequence at {:?}", now)
            }
        }
    }

    fn scd_failed(&mut self, op: Op, now: Uptime, health: &Health<'_>) {
        let scd = &mut self.scd;
        scd.failures += 1;
        scd.ready = false;
        scd.not_before = after(now, SETTLE);
        self.cycle.scd.failed = true;
        if matches!(op, Op::ScdStop | Op::ScdStart(_)) {
            scd.mode = ScdMode::Unknown;
        }
        match scd.phase {
            ScdPhase::Setup { recovery, .. } => {
                if recovery {
                    health.sensor_reinit(SensorId::Scd41, false);
                }
                scd.phase = ScdPhase::Backoff {
                    until: after(now, REINIT_BACKOFF),
                };
                self.cycle.scd.done = true;
            }
            // Перезапуск оборвался посреди stop/(set)/start: состояние датчика
            // неясно — со следующего цикла переинициализация.
            ScdPhase::SlowDown { .. } | ScdPhase::Calibrate { .. } => {
                scd.phase = ScdPhase::Setup {
                    recovery: true,
                    step:     0,
                };
                self.cycle.scd.done = true;
            }
            ScdPhase::Measuring => {
                if scd.failures >= REINIT_AFTER {
                    scd.phase = ScdPhase::Setup {
                        recovery: true,
                        step:     0,
                    };
                    self.cycle.scd.done = true;
                }
            }
            ScdPhase::Backoff { .. } => unreachable!("no SCD41 op runs during backoff"),
        }
    }

    fn baro_failed(&mut self, op: Op, now: Uptime, health: &Health<'_>) {
        let baro = &mut self.baro;
        baro.failures += 1;
        self.cycle.baro.failed = true;
        self.cycle.baro.done = true;
        match (op, baro.phase) {
            (Op::BaroInit, BaroPhase::NeedsInit { recovery }) => {
                if recovery {
                    health.sensor_reinit(SensorId::Bmp390, false);
                }
                baro.phase = BaroPhase::Backoff {
                    until: after(now, REINIT_BACKOFF),
                };
            }
            (Op::BaroPoll, BaroPhase::Ready) => {
                if baro.failures >= REINIT_AFTER {
                    baro.phase = BaroPhase::NeedsInit { recovery: true };
                }
            }
            (op, phase) => unreachable!("{op:?} failed in {phase:?}"),
        }
    }

    fn take_scd(&mut self, raw: ScdRaw, now: Uptime, health: &Health<'_>) {
        let c = &mut self.cycle;
        c.sample.co2 = Some(Ppm(raw.co2));
        let temp = TEMP_C
            .contains(&raw.temp_c)
            .then(|| MilliCelsius::from_celsius(raw.temp_c))
            .flatten();
        let humidity = HUMIDITY_PCT
            .contains(&raw.humidity)
            .then(|| MilliPercentRh::from_percent(raw.humidity))
            .flatten();
        c.sample.scd_temp = temp;
        c.sample.humidity = humidity;
        c.scd_temp_raw = temp.map(|_| raw.temp_c);
        if temp.is_none() || humidity.is_none() {
            health.sensor_fault(SensorId::Scd41, Op::ScdRead.code(), Fault::BadValue, now);
        }
    }

    fn take_baro(&mut self, raw: BaroRaw, now: Uptime, health: &Health<'_>) {
        let c = &mut self.cycle;
        let pressure = BARO_PRESSURE_PA
            .contains(&raw.pressure_pa)
            .then(|| CentiPascal::from_pascal(raw.pressure_pa))
            .flatten();
        let temp = TEMP_C
            .contains(&raw.temp_c)
            .then(|| MilliCelsius::from_celsius(raw.temp_c))
            .flatten();
        c.sample.pressure = pressure;
        c.sample.baro_temp = temp;
        c.baro_temp_raw = temp.map(|_| raw.temp_c);
        c.pressure_hpa = pressure.and_then(|_| scd_pressure(raw.pressure_pa));
        if pressure.is_none() || temp.is_none() {
            health.sensor_fault(SensorId::Bmp390, Op::BaroPoll.code(), Fault::BadValue, now);
        }
    }

    /// Закрыть цикл: протухшие датчики, итог, новый цикл с `now`.
    fn finish(&mut self, now: Uptime, health: &Health<'_>) -> Cycle {
        let c = self.cycle;
        let scd_stale = self.scd.phase == ScdPhase::Measuring
            && c.sample.co2.is_none()
            && !c.scd.failed
            && now.0.saturating_sub(self.scd.last_data.0) >= SCD_STALE;
        let baro_stale = self.baro.phase == BaroPhase::Ready
            && c.sample.pressure.is_none()
            && c.sample.baro_temp.is_none()
            && !c.baro.failed
            && now.0.saturating_sub(self.baro.last_data.0) >= BARO_STALE;
        if scd_stale {
            health.sensor_fault(SensorId::Scd41, Op::ScdDataReady.code(), Fault::Stale, now);
            self.scd.failures += 1;
            if self.scd.failures >= REINIT_AFTER {
                self.scd.phase = ScdPhase::Setup {
                    recovery: true,
                    step:     0,
                };
            }
        }
        if baro_stale {
            health.sensor_fault(SensorId::Bmp390, Op::BaroPoll.code(), Fault::Stale, now);
            self.baro.failures += 1;
            if self.baro.failures >= REINIT_AFTER {
                self.baro.phase = BaroPhase::NeedsInit { recovery: true };
            }
        }
        let periph_ok =
            c.sample.co2.is_some() && self.baro.phase == BaroPhase::Ready && !c.baro.failed && !baro_stale;
        self.cycle = CycleState::new(now);
        Cycle {
            sample: c.sample,
            periph_ok,
        }
    }
}

/// Смещение температуры SCD41, при котором он покажет температуру BMP390:
/// `T_scd − T_bmp + offset_old` (датчик вычитает смещение из своей сырой
/// температуры). Отрицательное датчик не принимает ⇒ 0.
fn calibrated_offset(scd_c: f32, bmp_c: f32, old: MilliCelsius) -> Option<MilliCelsius> {
    let old_c = old.celsius();
    #[allow(clippy::cast_possible_truncation)]
    let new_c = (f64::from(scd_c) - f64::from(bmp_c) + old_c).max(0.0) as f32;
    MilliCelsius::from_celsius(new_c)
}

/// Давление BMP390 → целые гПа для SCD41; вне 700–1200 гПа — не компенсируем.
fn scd_pressure(pa: f32) -> Option<Hectopascal> {
    let hpa = pa / 100.0;
    if !SCD_PRESSURE_HPA.contains(&hpa) {
        return None;
    }
    // Диапазон проверен: 700..=1200 влезает в u16, дробь отбрасываем.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(Hectopascal(hpa as u16))
}
