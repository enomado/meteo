use portable_atomic::{AtomicU8, AtomicU16, Ordering};

use embassy_time::{Duration, Timer};
use esp_hal::gpio::DriveMode;
use esp_hal::ledc::{
    LowSpeed,
    channel::{self, ChannelIFace},
    timer,
};

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

    /// Ждать завершения fade
    pub fn wait_fade(&self) {
        while self.r.is_duty_fade_running()
            || self.g.is_duty_fade_running()
            || self.b.is_duty_fade_running()
        {}
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

    /// Тест при старте: плавные переливы R → G → B → W → off
    pub fn startup_test(&mut self) {
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
            self.wait_fade();
        }
    }
}

/// Цвет blink-кода ошибки (в процентах яркости)
fn error_color(bit: u8) -> (u8, u8, u8) {
    const B: u8 = 40;
    match bit {
        SYS_BUF_OVERFLOW => (B, B / 2, 0), // оранжевый — warning
        SYS_NO_TCP => (0, 0, B),           // синий — нет сервера
        SYS_NO_PERIPH => (B, 0, B),        // фиолетовый — нет периферии
        SYS_NO_WIFI => (B, B, 0),          // жёлтый — нет wifi/ntp
        _ => (B, 0, 0),
    }
}

/// Основной LED-таск: показывает CO2 пока статус чист, иначе циклит blink-коды
#[embassy_executor::task]
pub async fn led_loop(mut led: RgbLed<'static>) {
    led.startup_test();

    loop {
        let status = SYSTEM_STATUS.load(Ordering::Relaxed);

        if status == 0 {
            let co2 = LATEST_CO2.load(Ordering::Relaxed);
            if co2 != u16::MAX {
                led.set_co2(co2);
            } else {
                led.fade_to(0, 0, 0, 200);
            }
            Timer::after(Duration::from_millis(2000)).await;
            continue;
        }

        // error mode: цикл по активным битам от 1× к 4×
        for bit_idx in 0u8..4 {
            let bit = 1u8 << bit_idx;
            // перечитываем статус — если бит сняли пока крутились, скипаем
            if SYSTEM_STATUS.load(Ordering::Relaxed) & bit == 0 {
                continue;
            }
            let blinks = bit_idx + 1;
            let (r, g, b) = error_color(bit);
            for _ in 0..blinks {
                led.set(r, g, b);
                Timer::after(Duration::from_millis(180)).await;
                led.set(0, 0, 0);
                Timer::after(Duration::from_millis(220)).await;
            }
            // пауза между разными кодами
            Timer::after(Duration::from_millis(700)).await;
        }
        // длинная пауза перед следующим полным циклом
        Timer::after(Duration::from_millis(900)).await;
    }
}

/// CO2 ppm → (R%, G%, B%) в процентах 0-100
///
/// - 0-400: чистый зелёный
/// - 400-700: зелёный → жёлтый
/// - 700-1000: жёлтый → красный
/// - 1000+: красный
pub fn co2_to_rgb(co2: u16) -> (u8, u8, u8) {
    const MAX: u8 = 30;

    match co2 {
        0..=400 => (0, MAX, 0),
        401..=700 => {
            let ratio = ((co2 - 400) as f32) / 300.0;
            let red = (MAX as f32 * ratio) as u8;
            (red, MAX, 0)
        }
        701..=1000 => {
            let ratio = ((co2 - 700) as f32) / 300.0;
            let green = (MAX as f32 * (1.0 - ratio)) as u8;
            (MAX, green, 0)
        }
        _ => (MAX, 0, 0),
    }
}
