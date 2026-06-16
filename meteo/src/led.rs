use portable_atomic::{AtomicU8, AtomicU16, Ordering};

use embassy_time::{Duration, Timer};
use esp_hal::gpio::DriveMode;
use esp_hal::gpio::interconnect::PeripheralOutput;
use esp_hal::ledc::{
    self, LSGlobalClkSource, LowSpeed,
    channel::{self, ChannelIFace},
    timer::{self, TimerIFace},
};
use esp_hal::peripherals::LEDC;
use esp_hal::time::Rate;

// --- System status bits (bit_idx+1 = число миганий) ---
pub const SYS_BUF_OVERFLOW: u8 = 1 << 0; // 1× — буфер переполнен
pub const SYS_NO_TCP: u8 = 1 << 1; // 2× — нет TCP/сервера
pub const SYS_NO_PERIPH: u8 = 1 << 2; // 3× — нет периферии (BMP390/SCD41)
pub const SYS_NO_WIFI: u8 = 1 << 3; // 4× — нет WiFi/NTP

pub static SYSTEM_STATUS: AtomicU8 = AtomicU8::new(SYS_NO_WIFI);

/// u16::MAX = «данных ещё нет», LED светит off в normal mode
pub static LATEST_CO2: AtomicU16 = AtomicU16::new(u16::MAX);

pub fn set_status(bits: u8) {
    SYSTEM_STATUS.fetch_or(bits, Ordering::Relaxed);
}

pub fn clear_status(bits: u8) {
    SYSTEM_STATUS.fetch_and(!bits, Ordering::Relaxed);
}

pub fn publish_co2(co2: u16) {
    LATEST_CO2.store(co2, Ordering::Relaxed);
}

pub struct RgbLed<'a> {
    r: channel::Channel<'a, LowSpeed>,
    g: channel::Channel<'a, LowSpeed>,
    b: channel::Channel<'a, LowSpeed>,
    cur_r: u8,
    cur_g: u8,
    cur_b: u8,
}

/// Поднимает LEDC-таймер (5 кГц, 8-bit duty, APB-clock) и собирает RgbLed на трёх каналах.
/// Шаринг `ledc_inst`/`timer0` через mk_static (StaticCell) — функция предполагает однократный вызов.
pub fn init_rgb_led(
    ledc_per: LEDC<'static>,
    r_pin: impl PeripheralOutput<'static>,
    g_pin: impl PeripheralOutput<'static>,
    b_pin: impl PeripheralOutput<'static>,
) -> RgbLed<'static> {
    let ledc_inst = crate::mk_static!(ledc::Ledc<'static>, ledc::Ledc::new(ledc_per));
    ledc_inst.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let mut timer0 = ledc_inst.timer::<LowSpeed>(timer::Number::Timer0);
    timer0
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty8Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_hz(5000),
        })
        .unwrap();
    let timer0 = crate::mk_static!(timer::Timer<'static, LowSpeed>, timer0);

    RgbLed::new(ledc_inst, timer0, r_pin, g_pin, b_pin)
}

impl<'a> RgbLed<'a> {
    pub fn new(
        ledc: &'a esp_hal::ledc::Ledc<'a>,
        timer: &'a timer::Timer<'a, LowSpeed>,
        r_pin: impl esp_hal::gpio::interconnect::PeripheralOutput<'a>,
        g_pin: impl esp_hal::gpio::interconnect::PeripheralOutput<'a>,
        b_pin: impl esp_hal::gpio::interconnect::PeripheralOutput<'a>,
    ) -> Self {
        let ch_config = channel::config::Config {
            timer,
            duty_pct: 0,
            drive_mode: DriveMode::PushPull,
        };

        let mut r = ledc.channel(channel::Number::Channel0, r_pin);
        r.configure(ch_config).unwrap();

        let mut g = ledc.channel(channel::Number::Channel1, g_pin);
        g.configure(ch_config).unwrap();

        let mut b = ledc.channel(channel::Number::Channel2, b_pin);
        b.configure(ch_config).unwrap();

        Self {
            r,
            g,
            b,
            cur_r: 0,
            cur_g: 0,
            cur_b: 0,
        }
    }

    /// Установить яркость каждого канала (0-100%)
    pub fn set(&mut self, r: u8, g: u8, b: u8) {
        let _ = self.r.set_duty(r);
        let _ = self.g.set_duty(g);
        let _ = self.b.set_duty(b);
        self.cur_r = r;
        self.cur_g = g;
        self.cur_b = b;
    }

    pub fn off(&mut self) {
        self.set(0, 0, 0);
    }

    /// Плавный переход к новому цвету за duration_ms (аппаратный fade)
    pub fn fade_to(&mut self, r: u8, g: u8, b: u8, duration_ms: u16) {
        if self.cur_r != r {
            let _ = self.r.start_duty_fade(self.cur_r, r, duration_ms);
        }
        if self.cur_g != g {
            let _ = self.g.start_duty_fade(self.cur_g, g, duration_ms);
        }
        if self.cur_b != b {
            let _ = self.b.start_duty_fade(self.cur_b, b, duration_ms);
        }
        self.cur_r = r;
        self.cur_g = g;
        self.cur_b = b;
    }

    /// CO2 уровень → плавный RGB
    ///
    /// - 0-700 ppm: зелёный
    /// - 700-1000 ppm: зелёный → жёлтый
    /// - 1000-1500 ppm: жёлтый → красный
    /// - 1500+ ppm: красный
    pub fn set_co2(&mut self, co2: u16) {
        let (r, g, b) = co2_to_rgb(co2);
        self.fade_to(r, g, b, 500);
    }

    /// Тест при старте: плавные переливы R → G → B → W → off.
    /// async-версия: пауза через `Timer::after`, не блокирует executor —
    /// параллельно стартуют wifi/ntp/sensor таски.
    pub async fn startup_test(&mut self) {
        use esp_println::println;

        const D: u16 = 400;
        const B: u8 = 30;

        let sequence: [(u8, u8, u8, &str); 5] = [
            (B, 0, 0, "RED"),
            (0, B, 0, "GREEN"),
            (0, 0, B, "BLUE"),
            (B, B, B, "WHITE"),
            (0, 0, 0, "OFF"),
        ];

        for (r, g, b, name) in sequence {
            println!("LED test: {}", name);
            self.fade_to(r, g, b, D);
            Timer::after(Duration::from_millis(D as u64)).await;
        }
    }
}

/// Цвет blink-кода ошибки (в процентах яркости).
/// SYS_NO_TCP не имеет фиксированного цвета — мигает текущим CO2-цветом
/// (см. blink_color), чтобы "нет сервера" не перекрывал индикацию воздуха.
fn error_color(bit: u8) -> (u8, u8, u8) {
    const B: u8 = 40;
    match bit {
        SYS_BUF_OVERFLOW => (B, B / 2, 0), // оранжевый — warning
        SYS_NO_PERIPH => (B, 0, B),        // фиолетовый — нет периферии
        SYS_NO_WIFI => (B, B, 0),          // жёлтый — нет wifi/ntp
        _ => (B, 0, 0),
    }
}

/// Цвет конкретного blink-overlay'я. Для NO_TCP берём текущий CO2-цвет,
/// иначе фиксированный error_color.
fn blink_color(bit: u8) -> (u8, u8, u8) {
    if bit == SYS_NO_TCP {
        let co2 = LATEST_CO2.load(Ordering::Relaxed);
        if co2 != u16::MAX {
            return co2_to_rgb(co2);
        }
        // CO2 ещё неизвестен — fallback на старый синий
        const B: u8 = 40;
        return (0, 0, B);
    }
    error_color(bit)
}

/// Параметры breathing-пульса для повышенного CO2.
/// `period_ms` — длительность полного цикла dim→bright→dim.
/// `depth_pct` — насколько затемняется LED в нижней точке (0..100, где 100 = до полного off).
#[derive(Clone, Copy)]
struct PulseSpec {
    period_ms: u16,
    depth_pct: u8,
}

/// CO2 → параметры пульса. `None` = статичный цвет (нормальная атмосфера).
/// Чем выше CO2, тем быстрее и глубже дыхание.
fn pulse_params(co2: u16) -> Option<PulseSpec> {
    match co2 {
        0..=999 => None,
        1000..=1399 => Some(PulseSpec {
            period_ms: 4000,
            depth_pct: 35,
        }),
        1400..=1999 => Some(PulseSpec {
            period_ms: 2200,
            depth_pct: 55,
        }),
        2000..=2999 => Some(PulseSpec {
            period_ms: 1400,
            depth_pct: 75,
        }),
        _ => Some(PulseSpec {
            period_ms: 900,
            depth_pct: 90,
        }),
    }
}

fn scale_rgb(rgb: (u8, u8, u8), pct_of_full: u8) -> (u8, u8, u8) {
    let s = |c: u8| ((c as u16 * pct_of_full as u16) / 100) as u8;
    (s(rgb.0), s(rgb.1), s(rgb.2))
}

/// Как часто проигрывать статус-оверлей (потеря связи и пр. ошибки) поверх
/// CO2-сигнала. Настраиваемая «крутилка»: правится здесь, одним числом.
/// Сейчас 60с — ошибки сигналятся раз в минуту, не заглушая цвет/дыхание.
const OVERLAY_INTERVAL_MS: u32 = 60_000;

/// Сыграть blink-коды для активных бит из `only_bits` (приоритет по bit_idx).
async fn play_blink_codes(led: &mut RgbLed<'_>, only_bits: u8) {
    for bit_idx in 0u8..4 {
        let bit = 1u8 << bit_idx;
        // перечитываем актуальный статус — бит мог погаснуть пока играли предыдущий
        if SYSTEM_STATUS.load(Ordering::Relaxed) & only_bits & bit == 0 {
            continue;
        }
        let blinks = bit_idx + 1;
        let (r, g, b) = blink_color(bit);
        for _ in 0..blinks {
            led.set(r, g, b);
            Timer::after(Duration::from_millis(180)).await;
            led.set(0, 0, 0);
            Timer::after(Duration::from_millis(220)).await;
        }
        Timer::after(Duration::from_millis(700)).await;
    }
}

/// Сыграть один CO2-step (статика 2 сек или один breathing-цикл).
/// Возвращает фактически проведённое время в мс — для accumulated-таймера overlay.
async fn play_co2_step(led: &mut RgbLed<'_>, co2: u16) -> u32 {
    let bright = co2_to_rgb(co2);
    match pulse_params(co2) {
        None => {
            led.fade_to(bright.0, bright.1, bright.2, 500);
            Timer::after(Duration::from_millis(2000)).await;
            2000
        }
        Some(p) => {
            let dim = scale_rgb(bright, 100 - p.depth_pct);
            let half = p.period_ms / 2;
            // вдох
            led.fade_to(bright.0, bright.1, bright.2, half);
            Timer::after(Duration::from_millis(half as u64)).await;
            // выдох
            led.fade_to(dim.0, dim.1, dim.2, half);
            Timer::after(Duration::from_millis(half as u64)).await;
            p.period_ms as u32
        }
    }
}

/// Основной LED-таск. Единое правило:
/// - boot: startup_test (R→G→B→W→off, ~2с, async)
/// - есть CO2-данные → CO2 mode (цвет + breathing). Раз в `OVERLAY_INTERVAL_MS`
///   поверх вставляется blink-overlay для всех активных ошибок.
/// - нет CO2-данных (сенсор не пришёл / SYS_NO_PERIPH) → blink-only
#[embassy_executor::task]
pub async fn led_loop(mut led: RgbLed<'static>) {
    led.startup_test().await;

    let mut since_overlay_ms: u32 = 0;

    loop {
        let status = SYSTEM_STATUS.load(Ordering::Relaxed);
        let co2 = LATEST_CO2.load(Ordering::Relaxed);
        let has_co2 = co2 != u16::MAX && (status & SYS_NO_PERIPH) == 0;

        if !has_co2 {
            // CO2-канал нечем заполнять: играем blink активных бит
            // (или просто ждём, если ошибок нет и мы ждём первое чтение).
            led.fade_to(0, 0, 0, 200);
            if status != 0 {
                play_blink_codes(&mut led, status).await;
            } else {
                Timer::after(Duration::from_millis(2000)).await;
            }
            since_overlay_ms = 0;
            continue;
        }

        // CO2 mode — основной канал, работает всегда когда есть данные.
        since_overlay_ms += play_co2_step(&mut led, co2).await;

        // Каждые ~OVERLAY_INTERVAL_MS вставляем blink-overlay всех активных ошибок.
        // Перечитываем статус — бит мог появиться/исчезнуть пока играли CO2-step.
        let status_now = SYSTEM_STATUS.load(Ordering::Relaxed);
        if status_now != 0 && since_overlay_ms >= OVERLAY_INTERVAL_MS {
            led.fade_to(0, 0, 0, 200);
            Timer::after(Duration::from_millis(300)).await;
            play_blink_codes(&mut led, status_now).await;
            since_overlay_ms = 0;
        }
    }
}

/// Линейная интерполяция в u8: a→b по t∈[0,1].
fn lerp(a: f32, b: f32, t: f32) -> u8 {
    (a + (b - a) * t) as u8
}

/// Параметр t∈[0,1] для co2 в отрезке [lo, hi].
fn segment_t(co2: u16, lo: u16, hi: u16) -> f32 {
    ((co2 - lo) as f32) / ((hi - lo) as f32)
}

/// CO2 ppm → (R%, G%, B%), яркость в долях `MAX`.
///
/// Детализированная зона 400-700 — основная зона мониторинга проветривания.
/// - 0-400: чистый зелёный
/// - 400-700: зелёный → жёлто-зелёный → жёлтый (мягкий gradient для тонкой оценки)
/// - 700-1000: жёлтый → оранжевый
/// - 1000-1500: оранжевый → красный (тут включается breathing pulse)
/// - 1500+: чистый красный
pub fn co2_to_rgb(co2: u16) -> (u8, u8, u8) {
    const MAX: u8 = 30;
    let m = MAX as f32;

    match co2 {
        0..=400 => (0, MAX, 0),
        // зелёный → жёлтый: красный канал поднимается 0 → MAX
        401..=700 => (lerp(0.0, m, segment_t(co2, 400, 700)), MAX, 0),
        // жёлтый → оранжевый: зелёный канал падает MAX → MAX/2
        701..=1000 => (MAX, lerp(m, m * 0.5, segment_t(co2, 700, 1000)), 0),
        // оранжевый → красный: зелёный канал падает MAX/2 → 0
        1001..=1500 => (MAX, lerp(m * 0.5, 0.0, segment_t(co2, 1000, 1500)), 0),
        _ => (MAX, 0, 0),
    }
}
