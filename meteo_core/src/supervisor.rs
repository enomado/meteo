//! Решение watchdog'а и причина reset — без железа. Прошивка кормит RWDT по
//! вердикту `Supervisor::tick` и сводит регистр reset reason к `ResetKind`.
//!
//! Семантика: ресетим, когда КОД ТАСКИ перестал исполняться, а НЕ когда внешняя
//! сеть лежит. Обрыв WiFi/сервера на часы — не вина устройства: retry-циклы
//! продолжают бить heartbeat. Heartbeat = признак жизни таски, а не успеха
//! передачи.

use core::time::Duration;

/// Время с момента boot. Монотонное; не путать с wall-clock (`EpochMillis`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Uptime(pub Duration);

/// Счётчик итераций таски. Значение не важно — важно, что оно меняется.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Heartbeat(pub u32);

/// Максимальный НОРМАЛЬНЫЙ простой heartbeat'а sensor-таски: период ~30с (25с
/// сон + ≤10с ожидания SCD41) → 90с = 3× запас.
pub const SENSOR_STALL_LIMIT: Duration = Duration::from_secs(90);

/// network: итерация цикла ≤ 5с сна + отправка датаграммы ≤ `SEND_TIMEOUT`
/// (5с). Лимит остался от TCP-отправителя (connect висел до 120с): снижать
/// его — отдельное решение, запас не мешает.
pub const NET_STALL_LIMIT: Duration = Duration::from_secs(180);

/// Надзор за одной таской.
struct Liveness {
    limit:       Duration,
    seen:        Heartbeat,
    last_change: Uptime,
    /// Пока таска не отметилась первый раз, лимит не действует: boot и
    /// калибровка BMP390+SCD41 могут занять ~минуту — это не зависание.
    armed:       bool,
}

impl Liveness {
    fn new(limit: Duration, now: Uptime, hb: Heartbeat) -> Self {
        Self {
            limit,
            seen: hb,
            last_change: now,
            armed: false,
        }
    }

    /// Учесть свежий heartbeat; `true` — таска жива.
    fn observe(&mut self, now: Uptime, hb: Heartbeat) -> bool {
        if hb != self.seen {
            self.seen = hb;
            self.last_change = now;
            self.armed = true;
        }
        !self.armed || now.0.saturating_sub(self.last_change.0) < self.limit
    }
}

/// Решение на один тик надзора.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Обе таски живы — кормим RWDT.
    Feed,
    /// Кто-то встал — НЕ кормим, железо ресетит по своему таймауту. Если таска
    /// оживёт раньше, следующий тик снова даст `Feed`.
    Withhold { sensor_ok: bool, net_ok: bool },
}

/// Супервизор sensor- и network-тасок.
pub struct Supervisor {
    sensor: Liveness,
    net:    Liveness,
}

impl Supervisor {
    /// `sensor`/`net` — значения heartbeat'ов в момент старта надзора.
    pub fn new(now: Uptime, sensor: Heartbeat, net: Heartbeat) -> Self {
        Self {
            sensor: Liveness::new(SENSOR_STALL_LIMIT, now, sensor),
            net:    Liveness::new(NET_STALL_LIMIT, now, net),
        }
    }

    pub fn tick(&mut self, now: Uptime, sensor: Heartbeat, net: Heartbeat) -> Verdict {
        let sensor_ok = self.sensor.observe(now, sensor);
        let net_ok = self.net.observe(now, net);
        if sensor_ok && net_ok {
            Verdict::Feed
        } else {
            Verdict::Withhold { sensor_ok, net_ok }
        }
    }
}

/// Причина, по которой чип перезагрузился. `match` на буте обязан быть
/// исчерпывающим, иначе новый вариант тихо уедет в «clean start».
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BootFault {
    /// Штатный старт: подача питания, кнопка reset, прошивка, чистая перезагрузка.
    Clean,
    /// Reset вызван паникой (`custom_halt`).
    Panic,
    /// Reset вызван RTC watchdog'ом (таска или весь executor встал).
    WdtStall,
}

/// Класс аппаратной причины reset — ровно то, что нужно для решения. Прошивка
/// сводит к нему `SocResetReason` чипа.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResetKind {
    /// Подача питания: RTC-память — мусор.
    PowerOn,
    /// RTC watchdog (любой уровень: CPU, core, system).
    RtcWatchdog,
    /// Программный reset (`software_reset`): штатный или после паники.
    Software,
    /// Остальное (brownout, super-WDT, glitch, USB/JTAG, недокументированный
    /// код). Отдельный вариант заводим, когда увидим такую причину в поле.
    Other,
}

/// Причина boot'а по классу reset и RTC-маркеру паники. Маркер нужен только
/// для паники: по железу она неотличима от штатного программного reset.
/// Столл по маркеру не определить — вставший executor его не запишет.
pub fn classify(kind: ResetKind, panic_marked: bool) -> BootFault {
    match kind {
        ResetKind::RtcWatchdog => BootFault::WdtStall,
        ResetKind::Software if panic_marked => BootFault::Panic,
        ResetKind::Software | ResetKind::PowerOn | ResetKind::Other => BootFault::Clean,
    }
}
