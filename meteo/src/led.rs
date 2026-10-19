#![allow(
    clippy::future_not_send,
    reason = "ESP32-C3 runs LED futures locally on one thread-mode Embassy executor; they are not sent between threads."
)]

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{
    Duration,
    Timer,
    with_timeout,
};
use esp_hal::gpio::DriveMode;
use esp_hal::gpio::interconnect::PeripheralOutput;
use esp_hal::ledc::channel::{
    self,
    ChannelIFace,
};
use esp_hal::ledc::timer::{
    self,
    TimerIFace,
};
use esp_hal::ledc::{
    self,
    LSGlobalClkSource,
    LowSpeed,
};
use esp_hal::peripherals::LEDC;
use esp_hal::time::Rate;
use meteo_core::codec::Ppm;
use meteo_core::led_color::{
    Rgb,
    co2_to_rgb,
    pulse_params,
};
use portable_atomic::{
    AtomicU8,
    AtomicU32,
    Ordering,
};

// --- System status bits (bit_idx+1 = число миганий) ---
pub const SYS_BUF_OVERFLOW: u8 = 1 << 0; // 1× — буфер переполнен
pub const SYS_NO_SERVER: u8 = 1 << 1; // 2× — сервер не подтверждает данные
pub const SYS_NO_PERIPH: u8 = 1 << 2; // 3× — нет периферии (BMP390/SCD41)
pub const SYS_NO_WIFI: u8 = 1 << 3; // 4× — нет WiFi/NTP
// Латч-биты (не текущий статус, а «этот boot после аварийного reset»): ставятся
// один раз на буте по причине reset и НЕ гаснут до передёрга питания — чтобы факт
// падения был виден визуально, даже без serial. См. crate::watchdog.
pub const SYS_PANIC_RECOVERED: u8 = 1 << 4; // 5× белым — был reset из-за паники
pub const SYS_WDT_RECOVERED: u8 = 1 << 5; // 6× голубым — был reset из-за зависания (watchdog)

/// Активные статус-биты; на старте пусто.
///
/// Пока идёт первая попытка подключения, «нет Wi-Fi» — ещё не сбой, и жёлтый
/// код до первого цвета воздуха только пугал. `SYS_NO_WIFI` ставит
/// `network::connection` по обрыву или со второй неудачной попытки подряд
/// (`meteo_core::wifi_retry`).
pub static SYSTEM_STATUS: AtomicU8 = AtomicU8::new(0);

/// Код отсутствия занимает значение за пределами диапазона `Ppm`.
/// `u32` атомик сохраняет все 65536 возможных значений CO2 без потери.
const NO_CO2: u32 = u32::MAX;

static LATEST_CO2: AtomicU32 = AtomicU32::new(NO_CO2);

/// Пришло свежее CO2: будит LED-таск из паузы blink-only, чтобы цвет воздуха
/// появился сразу, а не после паузы.
static CO2_ARRIVED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Яркость blink-кодов (проценты). Заметно выше CO2-палитры: код ошибки должен
/// читаться с другого конца комнаты.
const BLINK_LEVEL: u8 = 40;

/// Описание одного статус-бита. Единственный источник правды: маска, имя для
/// serial-лога и цвет кода раньше лежали в трёх раздельных `match`-ах, и
/// добавление бита требовало правки каждого (плюс магической «6» в двух циклах).
struct StatusBit {
    mask:  u8,
    /// Имя для serial-лога: LED-код на глаз расшифровывается плохо («были
    /// мигания, но не понял причину»), поэтому дублируем словами.
    name:  &'static str,
    /// `None` — мигать текущим CO2-цветом, см. [`blink_color`].
    color: Option<Rgb>,
}

impl StatusBit {
    /// Число миганий кода. Выводится из позиции бита ⇒ инвариант
    /// «`bit_idx + 1` = число миганий» держится по построению, а не по договору.
    const fn blinks(&self) -> u32 {
        self.mask.trailing_zeros() + 1
    }
}

/// Порядок = приоритет проигрывания (по возрастанию номера бита).
/// `SYS_NO_SERVER` цвета не имеет намеренно: мигает текущим CO2-цветом, чтобы «нет
/// сервера» не перекрывало индикацию воздуха.
const STATUS_BITS: [StatusBit; 6] = [
    StatusBit {
        mask:  SYS_BUF_OVERFLOW,
        name:  "BUF_OVERFLOW",
        color: Some(Rgb::new(BLINK_LEVEL, BLINK_LEVEL / 2, 0)), // оранжевый — warning
    },
    StatusBit {
        mask:  SYS_NO_SERVER,
        name:  "NO_SERVER",
        color: None, // текущий CO2-цвет
    },
    StatusBit {
        mask:  SYS_NO_PERIPH,
        name:  "NO_PERIPH",
        color: Some(Rgb::new(BLINK_LEVEL, 0, BLINK_LEVEL)), // фиолетовый — нет периферии
    },
    StatusBit {
        mask:  SYS_NO_WIFI,
        name:  "NO_WIFI",
        color: Some(Rgb::new(BLINK_LEVEL, BLINK_LEVEL, 0)), // жёлтый — нет wifi/ntp
    },
    StatusBit {
        mask:  SYS_PANIC_RECOVERED,
        name:  "PANIC_RECOVERED",
        color: Some(Rgb::new(BLINK_LEVEL, BLINK_LEVEL, BLINK_LEVEL)), // белый — после паники
    },
    StatusBit {
        mask:  SYS_WDT_RECOVERED,
        name:  "WDT_RECOVERED",
        color: Some(Rgb::new(0, BLINK_LEVEL, BLINK_LEVEL)), // голубой — после зависания
    },
];

/// Логируем только РЕАЛЬНО изменившиеся биты: `set_status` зовётся в циклах
/// (каждый неудачный коннект), и лог «текущего состояния» захлебнулся бы.
fn log_status_change(changed: u8, added: bool) {
    for status in &STATUS_BITS {
        if changed & status.mask != 0 {
            esp_println::println!(
                "led: {} {} ({}x blink)",
                if added { "SET" } else { "CLEAR" },
                status.name,
                status.blinks()
            );
        }
    }
}

pub fn set_status(bits: u8) {
    let prev = SYSTEM_STATUS.fetch_or(bits, Ordering::Relaxed);
    log_status_change(bits & !prev, true);
}

pub fn clear_status(bits: u8) {
    let prev = SYSTEM_STATUS.fetch_and(!bits, Ordering::Relaxed);
    log_status_change(bits & prev, false);
}

pub fn publish_co2(co2: Ppm) {
    LATEST_CO2.store(u32::from(co2.0), Ordering::Relaxed);
    CO2_ARRIVED.signal(());
}

/// Показание CO2 протухло: SCD41 не отдал измерение в этом цикле.
///
/// LED уходит в
/// blink-only, пока сенсор не оживёт. Отдельный канал, а не `SYS_NO_PERIPH`:
/// тот бит поднимает и сбой барометра, при котором CO2 остаётся живым.
pub fn clear_co2() {
    LATEST_CO2.store(NO_CO2, Ordering::Relaxed);
}

/// Последнее измерение CO2, `None` — свежего показания нет.
///
/// # Panics
///
/// Паникует, если атомарное хранилище содержит значение вне диапазона `Ppm`.
pub fn latest_co2() -> Option<Ppm> {
    match LATEST_CO2.load(Ordering::Relaxed) {
        NO_CO2 => None,
        co2 => Some(Ppm(u16::try_from(co2).expect("stored CO2 values fit in u16"))),
    }
}

pub struct RgbLed<'a> {
    r:   channel::Channel<'a, LowSpeed>,
    g:   channel::Channel<'a, LowSpeed>,
    b:   channel::Channel<'a, LowSpeed>,
    cur: Rgb,
}

/// GPIO outputs wired to the red, green, and blue LED channels.
pub struct RgbPins<R, G, B> {
    pub red:   R,
    pub green: G,
    pub blue:  B,
}

/// Поднимает `LEDC`-таймер (5 кГц, 8-bit duty, APB clock) и собирает `RgbLed` на трёх каналах.
/// Шаринг `ledc_inst`/`timer0` через `mk_static` (`StaticCell`) — функция предполагает однократный вызов.
///
/// # Panics
///
/// Паникует, если конфигурация аппаратного LEDC-таймера завершится ошибкой.
pub fn init_rgb_led<R, G, B>(ledc_per: LEDC<'static>, pins: RgbPins<R, G, B>) -> RgbLed<'static>
where
    R: PeripheralOutput<'static>,
    G: PeripheralOutput<'static>,
    B: PeripheralOutput<'static>,
{
    let ledc_inst = crate::mk_static!(ledc::Ledc<'static>, ledc::Ledc::new(ledc_per));
    ledc_inst.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let mut timer0 = ledc_inst.timer::<LowSpeed>(timer::Number::Timer0);
    timer0
        .configure(timer::config::Config {
            duty:         timer::config::Duty::Duty8Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency:    Rate::from_hz(5000),
        })
        .unwrap();
    let timer0 = crate::mk_static!(timer::Timer<'static, LowSpeed>, timer0);

    RgbLed::new(ledc_inst, timer0, pins)
}

impl<'a> RgbLed<'a> {
    /// Собирает `RgbLed` из трёх каналов LEDC.
    ///
    /// # Panics
    ///
    /// Паникует, если конфигурация любого канала завершится ошибкой.
    pub fn new<R, G, B>(
        ledc: &'a esp_hal::ledc::Ledc<'a>,
        timer: &'a timer::Timer<'a, LowSpeed>,
        pins: RgbPins<R, G, B>,
    ) -> Self
    where
        R: PeripheralOutput<'a>,
        G: PeripheralOutput<'a>,
        B: PeripheralOutput<'a>,
    {
        let ch_config = channel::config::Config {
            timer,
            duty_pct: 0,
            drive_mode: DriveMode::PushPull,
        };

        let mut r = ledc.channel(channel::Number::Channel0, pins.red);
        r.configure(ch_config).unwrap();

        let mut g = ledc.channel(channel::Number::Channel1, pins.green);
        g.configure(ch_config).unwrap();

        let mut b = ledc.channel(channel::Number::Channel2, pins.blue);
        b.configure(ch_config).unwrap();

        Self {
            r,
            g,
            b,
            cur: Rgb::OFF,
        }
    }

    /// Установить яркость каждого канала (0-100%)
    pub fn set(&mut self, color: Rgb) {
        let _ = self.r.set_duty(color.r);
        let _ = self.g.set_duty(color.g);
        let _ = self.b.set_duty(color.b);
        self.cur = color;
    }

    /// Плавный переход к новому цвету за `duration_ms` (аппаратный fade).
    pub fn fade_to(&mut self, color: Rgb, duration_ms: u16) {
        if self.cur.r != color.r {
            let _ = self.r.start_duty_fade(self.cur.r, color.r, duration_ms);
        }
        if self.cur.g != color.g {
            let _ = self.g.start_duty_fade(self.cur.g, color.g, duration_ms);
        }
        if self.cur.b != color.b {
            let _ = self.b.start_duty_fade(self.cur.b, color.b, duration_ms);
        }
        self.cur = color;
    }

    /// Тест при старте: плавные переливы R → G → B → W → off.
    /// async-версия: пауза через `Timer::after`, не блокирует executor —
    /// параллельно стартуют wifi/ntp/sensor таски.
    pub async fn startup_test(&mut self) {
        use esp_println::println;

        const D: u16 = 400;
        const B: u8 = 30;

        let sequence: [(Rgb, &str); 5] = [
            (Rgb::new(B, 0, 0), "RED"),
            (Rgb::new(0, B, 0), "GREEN"),
            (Rgb::new(0, 0, B), "BLUE"),
            (Rgb::new(B, B, B), "WHITE"),
            (Rgb::OFF, "OFF"),
        ];

        for (color, name) in sequence {
            println!("LED test: {}", name);
            self.fade_to(color, D);
            Timer::after(Duration::from_millis(u64::from(D))).await;
        }
    }
}

/// Цвет конкретного blink-overlay'я. Бит без фиксированного цвета мигает текущим
/// CO2-цветом; пока CO2 неизвестен — синим.
fn blink_color(status: &StatusBit) -> Rgb {
    status
        .color
        .unwrap_or_else(|| latest_co2().map_or_else(|| Rgb::new(0, 0, BLINK_LEVEL), co2_to_rgb))
}

/// Как часто проигрывать статус-оверлей (потеря связи и пр. ошибки) поверх
/// CO2-сигнала. Настраиваемая «крутилка»: правится здесь, одним числом.
/// Сейчас 60с — ошибки сигналятся раз в минуту, не заглушая цвет/дыхание.
const OVERLAY_INTERVAL_MS: u32 = 60_000;

/// Пауза МЕЖДУ сериями blink'ов в режиме «CO2-данных ещё нет» (старт: SCD41
/// греется/калибруется десятки секунд). Без неё серии играли встык и это
/// выглядело как непрерывное мигание (6× голубой = 2.4с работы на 0.7с паузы).
/// Короче оверлея: в blink-only индикатор больше ничего не показывает, и минута
/// темноты была бы неотличима от «LED сдох».
const BLINK_ONLY_GAP_MS: u64 = 8_000;

/// Сыграть blink-коды для активных бит из `only_bits` (приоритет по номеру бита).
async fn play_blink_codes(led: &mut RgbLed<'_>, only_bits: u8) {
    for status in &STATUS_BITS {
        // перечитываем актуальный статус — бит мог погаснуть пока играли предыдущий
        if SYSTEM_STATUS.load(Ordering::Relaxed) & only_bits & status.mask == 0 {
            continue;
        }
        esp_println::println!("led: blink {}x {}", status.blinks(), status.name);
        let color = blink_color(status);
        for _ in 0..status.blinks() {
            led.set(color);
            Timer::after(Duration::from_millis(180)).await;
            led.set(Rgb::OFF);
            Timer::after(Duration::from_millis(220)).await;
        }
        Timer::after(Duration::from_millis(700)).await;
    }
}

/// Сыграть один CO2-step (статика 2 сек или один breathing-цикл).
/// Возвращает фактически проведённое время в мс — для accumulated-таймера overlay.
async fn play_co2_step(led: &mut RgbLed<'_>, co2: Ppm) -> u32 {
    let bright = co2_to_rgb(co2);
    match pulse_params(co2) {
        None => {
            led.fade_to(bright, 500);
            Timer::after(Duration::from_millis(2000)).await;
            2000
        }
        Some(p) => {
            let dim = bright.scaled(100 - p.depth_pct);
            let half = p.period_ms / 2;
            // вдох
            led.fade_to(bright, half);
            Timer::after(Duration::from_millis(u64::from(half))).await;
            // выдох
            led.fade_to(dim, half);
            Timer::after(Duration::from_millis(u64::from(half))).await;
            u32::from(p.period_ms)
        }
    }
}

/// Основной LED-таск. Единое правило:
/// - boot: `startup_test` (R→G→B→W→off, ~2с, async)
/// - есть CO2-данные → CO2 mode (цвет + breathing). Раз в `OVERLAY_INTERVAL_MS`
///   поверх вставляется blink-overlay для всех активных ошибок.
/// - нет свежего CO2 (SCD41 не пришёл / сбойнул в последнем цикле) → blink-only
#[embassy_executor::task]
pub async fn led_loop(mut led: RgbLed<'static>) {
    led.startup_test().await;

    let mut since_overlay_ms: u32 = 0;

    loop {
        let status = SYSTEM_STATUS.load(Ordering::Relaxed);
        let Some(co2) = latest_co2() else {
            // CO2-канал нечем заполнять: играем blink активных бит
            // (или просто ждём, если ошибок нет и мы ждём первое чтение).
            // Сигнал, взведённый до этой ветки, — о показании, которое уже
            // протухло; ждём только пришедшее после.
            CO2_ARRIVED.reset();
            led.fade_to(Rgb::OFF, 200);
            let gap_ms = if status != 0 {
                play_blink_codes(&mut led, status).await;
                BLINK_ONLY_GAP_MS
            } else {
                2000
            };
            // Пришедшее CO2 обрывает паузу: цвет воздуха — сразу.
            let _ = with_timeout(Duration::from_millis(gap_ms), CO2_ARRIVED.wait()).await;
            since_overlay_ms = 0;
            continue;
        };

        // CO2 mode — основной канал, работает всегда когда есть данные.
        since_overlay_ms += play_co2_step(&mut led, co2).await;

        // Каждые ~OVERLAY_INTERVAL_MS вставляем blink-overlay всех активных ошибок.
        // Перечитываем статус — бит мог появиться/исчезнуть пока играли CO2-step.
        let status_now = SYSTEM_STATUS.load(Ordering::Relaxed);
        if status_now != 0 && since_overlay_ms >= OVERLAY_INTERVAL_MS {
            led.fade_to(Rgb::OFF, 200);
            Timer::after(Duration::from_millis(300)).await;
            play_blink_codes(&mut led, status_now).await;
            since_overlay_ms = 0;
        }
    }
}
