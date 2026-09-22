//! Self-healing supervision: паника → reset, зависание → RWDT reset.
//!
//! Две независимые страховки от «поработало и залипло навсегда». Мотивация —
//! инцидент 2026-07-04: паника в hot-path (`.unwrap()`) → esp-backtrace уходит в
//! `interrupt_free(loop{})` (прерывания ВЫКЛ, вечный цикл) → чип мёртв до
//! передёрга питания. Итог — 22ч тихой потери данных, ноль реконнектов.
//!
//! 1. **`custom_halt`** (фича esp-backtrace `custom-halt`) — вместо вечного
//!    loop{} после паники делаем `software_reset()`. Бэктрейс печатается ДО
//!    (esp-backtrace: pre_backtrace → печать → abort → custom_halt), поэтому
//!    defmt-лог паники успевает уйти в UART. Покрывает ВСЕ паники.
//!
//! 2. **RWDT hardware watchdog** — ловит НАСТОЯЩИЕ зависания (await, который
//!    никогда не резолвится в beta esp-radio; interrupt storm; столл executor'а),
//!    которые паникой НЕ являются, поэтому `custom_halt` их не видит. Кормится
//!    только пока критичные таски (sensor + network) реально крутят свои циклы.
//!
//! Семантика watchdog: ресетим когда КОД ТАСКИ перестал исполняться, а НЕ когда
//! внешняя сеть лежит. Обрыв WiFi/сервера на часы — это НЕ вина устройства:
//! retry-циклы продолжают бить heartbeat, и мы НЕ ресетимся. Heartbeat = признак
//! жизни таски, а не успеха передачи.

use embassy_time::{
    Duration,
    Instant,
    Timer,
};
use esp_hal::ram;
use esp_hal::rtc_cntl::{
    Rwdt,
    RwdtStage,
    SocResetReason,
};
use esp_hal::time::Duration as HalDuration;
use esp_println::println;
use portable_atomic::{
    AtomicU32,
    Ordering,
};

/// Счётчик итераций `sensor_loop`. Бампается раз в ~30с в норме.
pub static SENSOR_HB: AtomicU32 = AtomicU32::new(0);
/// Счётчик итераций `network_send_loop`. Бампается ≤5с даже когда сеть лежит
/// (connect fails → retry-loop всё равно крутится и бьёт heartbeat).
pub static NET_HB: AtomicU32 = AtomicU32::new(0);

#[inline]
pub fn beat_sensor() {
    SENSOR_HB.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn beat_net() {
    NET_HB.fetch_add(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Причина reset: железо + маркер паники (RTC fast RAM)
//
// Проблема: авто-reset (custom_halt / RWDT) чинит зависание, но ПРЯЧЕТ факт
// проблемы — без serial мы бы не узнали, что что-то падало. На буте определяем
// причину и мигаем отличимым LED-кодом.
//
// Оракул причины — регистр reset reason (`esp_hal::system::reset_reason`), а не
// запись «перед смертью»: столл, ради которого стоит RWDT, — это вставший
// executor, и watchdog-таска в нём же ⇒ писать маркер некому. RTC-маркер нужен
// только для паники: по железу она неотличима от штатного `software_reset`.
//
// portable_atomic::AtomicU32 реализует esp_hal::Persistable ⇒ кладём в
// persistent-секцию без `static mut`/unsafe. На холодном старте содержимое —
// мусор, поэтому валидность области проверяем магическим словом.
// ---------------------------------------------------------------------------

/// "mt" + версия формата. Отличает валидную область от мусора холодного старта.
/// Версия 2: слово причины хранит только маркер паники, счётчик ведёт бут.
const FAULT_MAGIC: u32 = 0x6D74_0002;

/// Значение `RTC_PANIC_MARK`, записанное `custom_halt` перед reset.
const PANIC_MARK: u32 = 1;

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

/// Итог определения причины boot'а.
pub struct BootReport {
    pub fault:                 BootFault,
    /// Сколько раз страховки срабатывали с последнего power-on, включая этот boot.
    pub faults_since_power_on: u32,
    /// Сырая причина от железа — для лога. `None`: код, не описанный в esp-hal.
    pub reset_reason:          Option<SocResetReason>,
}

#[ram(unstable(rtc_fast, persistent))]
static RTC_MAGIC: AtomicU32 = AtomicU32::new(0);
#[ram(unstable(rtc_fast, persistent))]
static RTC_PANIC_MARK: AtomicU32 = AtomicU32::new(0);
/// Сколько раз страховки срабатывали с последнего power-on (копится за сессию).
#[ram(unstable(rtc_fast, persistent))]
static RTC_FAULT_COUNT: AtomicU32 = AtomicU32::new(0);

/// Причина boot'а по регистру reset reason и маркеру паники.
///
/// Незнакомые причины (`SysSuperWdt`, brownout, glitch, недокументированный
/// код) — `Clean`: отдельный вариант заводим, когда увидим такую в поле; сырое
/// значение печатает бут-лог.
fn classify(reason: Option<SocResetReason>, panic_marked: bool) -> BootFault {
    match reason {
        Some(SocResetReason::SysRtcWdt | SocResetReason::CoreRtcWdt | SocResetReason::Cpu0RtcWdt) => {
            BootFault::WdtStall
        }
        Some(SocResetReason::CoreSw | SocResetReason::Cpu0Sw) if panic_marked => BootFault::Panic,
        _ => BootFault::Clean,
    }
}

/// Определить причину ТЕКУЩЕГО boot'а и перевзвести маркер на будущее.
/// Вызывать один раз в начале main.
///
/// Power-on или невалидная магия ⇒ область RTC инициализируется заново (счётчик
/// с нуля). Маркер паники гасится всегда, чтобы будущий штатный reset не показал
/// устаревшую причину. Счётчик растёт на буте, если причина — сбой: RWDT-reset
/// может стереть RTC-область, тогда счёт начнётся с этого сбоя.
pub fn take_boot_fault() -> BootReport {
    let reset_reason = esp_hal::system::reset_reason();

    let area_valid =
        RTC_MAGIC.load(Ordering::Relaxed) == FAULT_MAGIC && reset_reason != Some(SocResetReason::ChipPowerOn);
    if !area_valid {
        RTC_MAGIC.store(FAULT_MAGIC, Ordering::Relaxed);
        RTC_PANIC_MARK.store(0, Ordering::Relaxed);
        RTC_FAULT_COUNT.store(0, Ordering::Relaxed);
    }

    let panic_marked = RTC_PANIC_MARK.load(Ordering::Relaxed) == PANIC_MARK;
    RTC_PANIC_MARK.store(0, Ordering::Relaxed);

    let fault = classify(reset_reason, panic_marked);
    if fault != BootFault::Clean {
        RTC_FAULT_COUNT.fetch_add(1, Ordering::Relaxed);
    }

    BootReport {
        fault,
        faults_since_power_on: RTC_FAULT_COUNT.load(Ordering::Relaxed),
        reset_reason,
    }
}

/// Паника → reset вместо вечного `loop{}`. Зовётся esp-backtrace (фича
/// `custom-halt`) ПОСЛЕ печати бэктрейса. Прерывания в этой точке уже выключены
/// паник-хендлером; `software_reset()` их не требует. `#[no_mangle]` — символ
/// линкуется по имени из extern-декларации внутри esp-backtrace.
#[unsafe(no_mangle)]
extern "Rust" fn custom_halt() -> ! {
    // Пометить панику для LED-сигнала после reset (по железу это обычный
    // software reset), затем перезагрузиться. Магию ставит бут.
    RTC_PANIC_MARK.store(PANIC_MARK, Ordering::Relaxed);
    esp_hal::system::software_reset()
}

/// Максимальный НОРМАЛЬНЫЙ простой heartbeat'а, после которого таска считается
/// зависшей. sensor: период ~30с (25с сон + ~5с SCD) → 90с = 3× запас.
const SENSOR_STALL_LIMIT: Duration = Duration::from_secs(90);
/// network: цикл крутится ≤5с (send 3с / retry 5с), НО `socket.connect()` может
/// висеть до socket-timeout (120с) при недоступном сервере, а отправка пакета —
/// до `SEND_TIMEOUT` (60с) в ожидании ACK — это легитимно, не зависание.
/// Поэтому лимит > 120с с запасом.
const NET_STALL_LIMIT: Duration = Duration::from_secs(180);

/// Как часто проверяем liveness и кормим RWDT.
const FEED_INTERVAL: Duration = Duration::from_secs(10);

/// Hardware-таймаут RWDT Stage0 (действие по умолчанию = reset системы). Должен
/// быть > FEED_INTERVAL с большим запасом (иначе ложный ресет при джиттере
/// executor'а). 120с: после того как feeder ПЕРЕСТАЛ кормить (детект столла),
/// железо ресетит в пределах 120с.
const RWDT_TIMEOUT_SECS: u64 = 120;

/// Супервизор. Кормит RWDT пока обе таски живы; при зависании перестаёт кормить
/// → железо ресетит чип. Паники ловятся отдельно (`custom_halt`), сюда не
/// доходят.
#[embassy_executor::task]
pub async fn watchdog_loop(mut rwdt: Rwdt) {
    rwdt.set_timeout(RwdtStage::Stage0, HalDuration::from_secs(RWDT_TIMEOUT_SECS));
    rwdt.enable();
    println!("watchdog: RWDT armed, hw timeout {}s", RWDT_TIMEOUT_SECS);

    let now = Instant::now();
    let mut sensor_seen = SENSOR_HB.load(Ordering::Relaxed);
    let mut net_seen = NET_HB.load(Ordering::Relaxed);
    let mut sensor_last_change = now;
    let mut net_last_change = now;

    // Per-task arming: пока таска не отметилась первый раз, её лимит НЕ
    // enforcing (boot/калибровка BMP390+SCD41 может занять ~минуту — не ловим
    // это как зависание). Как только пробила первый heartbeat — включаем надзор.
    let mut sensor_armed = false;
    let mut net_armed = false;

    loop {
        Timer::after(FEED_INTERVAL).await;
        let now = Instant::now();

        let s = SENSOR_HB.load(Ordering::Relaxed);
        if s != sensor_seen {
            sensor_seen = s;
            sensor_last_change = now;
            sensor_armed = true;
        }
        let n = NET_HB.load(Ordering::Relaxed);
        if n != net_seen {
            net_seen = n;
            net_last_change = now;
            net_armed = true;
        }

        // Таска ОК если ещё не armed (boot-grace) ИЛИ heartbeat свежий.
        let sensor_ok = !sensor_armed || (now - sensor_last_change) < SENSOR_STALL_LIMIT;
        let net_ok = !net_armed || (now - net_last_change) < NET_STALL_LIMIT;

        if sensor_ok && net_ok {
            rwdt.feed();
        } else {
            // НЕ кормим → RWDT ресетит через свой hw-таймаут. Лог успеет уйти.
            // Причину следующий бут прочитает из регистра reset reason.
            println!(
                "watchdog: STALL sensor_ok={} net_ok={} → withholding feed, reset imminent",
                sensor_ok, net_ok
            );
        }
    }
}
