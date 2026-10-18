//! Self-healing supervision: паника → reset, зависание → RWDT reset.
//!
//! Две независимые страховки от «поработало и залипло навсегда». Мотивация —
//! инцидент 2026-07-04: паника в hot-path (`.unwrap()`) → esp-backtrace уходит в
//! `interrupt_free(loop{})` (прерывания ВЫКЛ, вечный цикл) → чип мёртв до
//! передёрга питания. Итог — 22ч тихой потери данных, ноль реконнектов.
//!
//! 1. **`custom_halt`** (фича esp-backtrace `custom-halt`) — вместо вечного
//!    loop{} после паники делаем `software_reset()`. Бэктрейс печатается ДО
//!    (`esp-backtrace`: `pre_backtrace` → печать → abort → `custom_halt`), поэтому
//!    defmt-лог паники успевает уйти в UART. Покрывает ВСЕ паники.
//!
//! 2. **RWDT hardware watchdog** — ловит НАСТОЯЩИЕ зависания (`await`, который
//!    никогда не резолвится в beta-версии `esp-radio`; шторм прерываний; столл executor'а),
//!    которые паникой НЕ являются, поэтому `custom_halt` их не видит. Кормится
//!    только пока критичные таски (sensor + network) реально крутят свои циклы.
//!
//! Семантика и лимиты надзора — `meteo_core::supervisor` (там же тесты).

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
use meteo_core::health::{
    Attach,
    Health,
    NetStage,
    WORDS,
    Word,
};
use meteo_core::sensors::OpCode;
use meteo_core::supervisor::{
    BootFault,
    Heartbeat,
    ResetKind,
    Supervisor,
    TaskHeartbeats,
    Uptime,
    Verdict,
};
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
// Журнал здоровья в RTC fast RAM: причина reset, счётчики сбоев, место зависания
//
// Проблема: авто-reset (custom_halt / RWDT) чинит зависание, но ПРЯЧЕТ факт
// проблемы — без serial мы бы не узнали, что что-то падало. На буте определяем
// причину, мигаем отличимым LED-кодом и пишем её в журнал, который уходит на
// сервер (`meteo_core::health`).
//
// Оракул причины — регистр reset reason (`esp_hal::system::reset_reason`), а не
// запись «перед смертью»: столл, ради которого стоит RWDT, может быть вставшим
// executor'ом, и watchdog-таска в нём же. Маркер паники нужен потому, что по
// железу паника неотличима от штатного `software_reset`.
//
// portable_atomic::AtomicU32 (и массив из них) реализует esp_hal::Persistable ⇒
// кладём в persistent-секцию без `static mut`/unsafe. На холодном старте
// содержимое — мусор: журнал проверяет свою магию сам.
// ---------------------------------------------------------------------------

/// Итог определения причины boot'а.
pub struct BootReport {
    pub fault:                 BootFault,
    /// Сколько раз страховки срабатывали с последнего power-on, включая этот boot.
    pub faults_since_power_on: u32,
    /// Сырая причина от железа — для лога. `None`: код, не описанный в esp-hal.
    pub reset_reason:          Option<SocResetReason>,
}

#[ram(unstable(rtc_fast, persistent))]
static RTC_HEALTH: [AtomicU32; WORDS] = [const { AtomicU32::new(0) }; WORDS];

/// Журнал здоровья (подключён на буте [`take_boot_fault`]).
#[must_use]
pub const fn health() -> Health<'static> {
    Health::view(&RTC_HEALTH)
}

/// Сводит причину reset ESP32-C3 к классу, по которому решает `classify`.
/// `match` исчерпывающий: новая причина в esp-hal не соберётся молча.
const fn reset_kind(reason: Option<SocResetReason>) -> ResetKind {
    let Some(reason) = reason else {
        return ResetKind::Other;
    };
    match reason {
        SocResetReason::ChipPowerOn => ResetKind::PowerOn,
        SocResetReason::SysRtcWdt | SocResetReason::CoreRtcWdt | SocResetReason::Cpu0RtcWdt => {
            ResetKind::RtcWatchdog
        }
        SocResetReason::CoreSw | SocResetReason::Cpu0Sw => ResetKind::Software,
        SocResetReason::CoreDeepSleep
        | SocResetReason::CoreMwdt0
        | SocResetReason::CoreMwdt1
        | SocResetReason::Cpu0Mwdt0
        | SocResetReason::Cpu0Mwdt1
        | SocResetReason::SysBrownOut
        | SocResetReason::SysSuperWdt
        | SocResetReason::SysClkGlitch
        | SocResetReason::CoreEfuseCrc
        | SocResetReason::CoreUsbUart
        | SocResetReason::CoreUsbJtag
        | SocResetReason::CorePwrGlitch => ResetKind::Other,
    }
}

/// Определить причину ТЕКУЩЕГО boot'а и учесть её в журнале.
/// Вызывать один раз в начале main, до спавна тасок.
///
/// Power-on или чужая магия ⇒ журнал обнуляется. Служебные слова прошлой
/// загрузки (маркер паники, этапы тасок) журнал гасит сам после разбора.
#[must_use]
pub fn take_boot_fault() -> BootReport {
    let reset_reason = esp_hal::system::reset_reason();
    let kind = reset_kind(reset_reason);
    let (health, attach) = Health::attach(&RTC_HEALTH, kind == ResetKind::PowerOn);
    if attach == Attach::Fresh && kind != ResetKind::PowerOn {
        println!("BOOT: health journal was not valid, started afresh");
    }
    // Код — аппаратный номер причины из TRM; 0 — esp-hal его не описывает.
    let fault = health.record_boot(kind, reset_reason.map_or(0, |r| r as u32));

    BootReport {
        fault,
        faults_since_power_on: health.faults_since_power_on(),
        reset_reason,
    }
}

/// Паника → reset вместо вечного `loop{}`. Зовётся esp-backtrace (фича
/// `custom-halt`) ПОСЛЕ печати бэктрейса. Прерывания в этой точке уже выключены
/// паник-хендлером; `software_reset()` их не требует. `#[no_mangle]` — символ
/// линкуется по имени из extern-декларации внутри esp-backtrace.
#[unsafe(no_mangle)]
extern "Rust" fn custom_halt() -> ! {
    // Пометить панику для LED-сигнала и журнала после reset (по железу это
    // обычный software reset), затем перезагрузиться.
    health().mark_panic();
    esp_hal::system::software_reset()
}

/// Как часто проверяем liveness и кормим RWDT.
const FEED_INTERVAL: Duration = Duration::from_secs(10);

/// Hardware-таймаут RWDT Stage0 (действие по умолчанию = reset системы). Должен
/// быть > `FEED_INTERVAL` с большим запасом (иначе ложный ресет при джиттере
/// executor'а). 120с: после того как feeder ПЕРЕСТАЛ кормить (детект столла),
/// железо ресетит в пределах 120с.
const RWDT_TIMEOUT_SECS: u64 = 120;

/// Время с boot по часам embassy (им же живут надзор и отправитель).
#[must_use]
pub fn uptime() -> Uptime {
    Uptime(core::time::Duration::from_micros(Instant::now().as_micros()))
}

fn heartbeats() -> TaskHeartbeats {
    TaskHeartbeats {
        sensor:  Heartbeat(SENSOR_HB.load(Ordering::Relaxed)),
        network: Heartbeat(NET_HB.load(Ordering::Relaxed)),
    }
}

/// Кормит RWDT по вердикту `Supervisor` (лимиты, boot-grace, оживление — там
/// же, с тестами на хосте). Паники ловятся отдельно (`custom_halt`), сюда не
/// доходят.
#[embassy_executor::task]
pub async fn watchdog_loop(mut rwdt: Rwdt) {
    rwdt.set_timeout(RwdtStage::Stage0, HalDuration::from_secs(RWDT_TIMEOUT_SECS));
    rwdt.enable();
    println!("watchdog: RWDT armed, hw timeout {}s", RWDT_TIMEOUT_SECS);

    let initial_heartbeats = heartbeats();
    let mut supervisor = Supervisor::new(uptime(), initial_heartbeats);
    let mut stall_recorded = false;

    loop {
        Timer::after(FEED_INTERVAL).await;

        let now = uptime();
        let current_heartbeats = heartbeats();
        match supervisor.tick(now, current_heartbeats) {
            Verdict::Feed => {
                rwdt.feed();
                // Время жизни — на случай, если встанет весь executor и
                // записать зависание будет некому.
                health().alive(now);
            }
            Verdict::Withhold { sensor_ok, net_ok } => {
                // НЕ кормим → RWDT ресетит через свой hw-таймаут. Лог успеет
                // уйти. Кто встал и на каком этапе — в журнал (один раз: дальше
                // этапы уже не меняются), причину следующий бут прочитает из
                // регистра reset reason.
                if !stall_recorded {
                    health().record_stall(sensor_ok, net_ok, now);
                    stall_recorded = true;
                }
                println!(
                    "watchdog: STALL sensor_ok={} net_ok={} sensor_stage={} net_stage={} → withholding feed, reset imminent",
                    sensor_ok,
                    net_ok,
                    OpCode(health().get(Word::SensorStage)).name(),
                    NetStage::name_of(health().get(Word::NetStage)),
                );
            }
        }
    }
}
