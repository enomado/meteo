use embassy_time::{
    Duration,
    Timer,
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
use portable_atomic::{
    AtomicU8,
    AtomicU16,
    Ordering,
};

// --- System status bits (bit_idx+1 = число миганий) ---
pub const SYS_BUF_OVERFLOW: u8 = 1 << 0; // 1× — буфер переполнен
pub const SYS_NO_TCP: u8 = 1 << 1; // 2× — нет TCP/сервера
pub const SYS_NO_PERIPH: u8 = 1 << 2; // 3× — нет периферии (BMP390/SCD41)
pub const SYS_NO_WIFI: u8 = 1 << 3; // 4× — нет WiFi/NTP
// Латч-биты (не текущий статус, а «этот boot после аварийного reset»): ставятся
// один раз на буте из RTC-маркера и НЕ гаснут до передёрга питания — чтобы факт
// падения был виден визуально, даже без serial. См. crate::watchdog.
pub const SYS_PANIC_RECOVERED: u8 = 1 << 4; // 5× белым — был reset из-за паники
pub const SYS_WDT_RECOVERED: u8 = 1 << 5; // 6× голубым — был reset из-за зависания (watchdog)

pub static SYSTEM_STATUS: AtomicU8 = AtomicU8::new(SYS_NO_WIFI);

/// Сентинел «CO2 ещё не измерялся»: атомик не умеет хранить `Option`, поэтому
/// отсутствие кодируем значением. Наружу отдаём честный `Option` — см.
/// [`latest_co2`], сравнений с `u16::MAX` в логике быть не должно.
const NO_CO2: u16 = u16::MAX;

static LATEST_CO2: AtomicU16 = AtomicU16::new(NO_CO2);

/// Яркость канала в процентах (0..=100) — ровно то, что принимает LEDC
/// `set_duty`. Держим триплет одной структурой: три параллельных `u8`
/// (cur_r/cur_g/cur_b плюс кортежи в таблицах цветов) слишком легко разъезжаются
/// и путаются местами.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const OFF: Rgb = Rgb::new(0, 0, 0);

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Затемнение до `pct` процентов от текущей яркости (100 = без изменений).
    fn scaled(self, pct: u8) -> Self {
        let s = |c: u8| ((c as u16 * pct as u16) / 100) as u8;
        Self::new(s(self.r), s(self.g), s(self.b))
    }
}

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
    /// «bit_idx + 1 = число миганий» держится по построению, а не по договору.
    fn blinks(&self) -> u32 {
        self.mask.trailing_zeros() + 1
    }
}

/// Порядок = приоритет проигрывания (по возрастанию номера бита).
/// SYS_NO_TCP цвета не имеет намеренно: мигает текущим CO2-цветом, чтобы «нет
/// сервера» не перекрывало индикацию воздуха.
const STATUS_BITS: [StatusBit; 6] = [
    StatusBit {
        mask:  SYS_BUF_OVERFLOW,
        name:  "BUF_OVERFLOW",
        color: Some(Rgb::new(BLINK_LEVEL, BLINK_LEVEL / 2, 0)), // оранжевый — warning
    },
    StatusBit {
        mask:  SYS_NO_TCP,
        name:  "NO_TCP",
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

/// Логируем только РЕАЛЬНО изменившиеся биты: set_status зовётся в циклах
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

pub fn publish_co2(co2: u16) {
    LATEST_CO2.store(co2, Ordering::Relaxed);
}

/// Последнее измерение CO2, `None` — сенсор ещё ничего не отдал.
pub fn latest_co2() -> Option<u16> {
    match LATEST_CO2.load(Ordering::Relaxed) {
        NO_CO2 => None,
        co2 => Some(co2),
    }
}

pub struct RgbLed<'a> {
    r:   channel::Channel<'a, LowSpeed>,
    g:   channel::Channel<'a, LowSpeed>,
    b:   channel::Channel<'a, LowSpeed>,
    cur: Rgb,
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
            duty:         timer::config::Duty::Duty8Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency:    Rate::from_hz(5000),
        })
        .unwrap();
    let timer0 = crate::mk_static!(timer::Timer<'static, LowSpeed>, timer0);

    RgbLed::new(ledc_inst, timer0, r_pin, g_pin, b_pin)
}

impl<'a> RgbLed<'a> {
    pub fn new(
        ledc: &'a esp_hal::ledc::Ledc<'a>,
        timer: &'a timer::Timer<'a, LowSpeed>,
        r_pin: impl PeripheralOutput<'a>,
        g_pin: impl PeripheralOutput<'a>,
        b_pin: impl PeripheralOutput<'a>,
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

    /// Плавный переход к новому цвету за duration_ms (аппаратный fade)
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
            Timer::after(Duration::from_millis(D as u64)).await;
        }
    }
}

/// Цвет конкретного blink-overlay'я. Бит без фиксированного цвета мигает текущим
/// CO2-цветом; пока CO2 неизвестен — синим.
fn blink_color(status: &StatusBit) -> Rgb {
    match status.color {
        Some(color) => color,
        None => {
            match latest_co2() {
                Some(co2) => co2_to_rgb(co2),
                None => Rgb::new(0, 0, BLINK_LEVEL),
            }
        }
    }
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
        1000..=1399 => {
            Some(PulseSpec {
                period_ms: 4000,
                depth_pct: 35,
            })
        }
        1400..=1999 => {
            Some(PulseSpec {
                period_ms: 2200,
                depth_pct: 55,
            })
        }
        2000..=2999 => {
            Some(PulseSpec {
                period_ms: 1400,
                depth_pct: 75,
            })
        }
        _ => {
            Some(PulseSpec {
                period_ms: 900,
                depth_pct: 90,
            })
        }
    }
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
async fn play_co2_step(led: &mut RgbLed<'_>, co2: u16) -> u32 {
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
            Timer::after(Duration::from_millis(half as u64)).await;
            // выдох
            led.fade_to(dim, half);
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
        // При SYS_NO_PERIPH последнее показание считаем протухшим: сенсор отвалился.
        let Some(co2) = latest_co2().filter(|_| status & SYS_NO_PERIPH == 0) else {
            // CO2-канал нечем заполнять: играем blink активных бит
            // (или просто ждём, если ошибок нет и мы ждём первое чтение).
            led.fade_to(Rgb::OFF, 200);
            if status != 0 {
                play_blink_codes(&mut led, status).await;
                Timer::after(Duration::from_millis(BLINK_ONLY_GAP_MS)).await;
            } else {
                Timer::after(Duration::from_millis(2000)).await;
            }
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

/// Линейная интерполяция в u8: a→b по t∈[0,1].
fn lerp(a: f32, b: f32, t: f32) -> u8 {
    (a + (b - a) * t) as u8
}

/// Параметр t∈[0,1] для co2 в отрезке [lo, hi].
fn segment_t(co2: u16, lo: u16, hi: u16) -> f32 {
    ((co2 - lo) as f32) / ((hi - lo) as f32)
}

/// CO2 ppm → цвет, яркость в долях `MAX`.
///
/// Детализированная зона 400-700 — основная зона мониторинга проветривания.
/// - 0-400: чистый зелёный
/// - 400-700: зелёный → жёлто-зелёный → жёлтый (мягкий gradient для тонкой оценки)
/// - 700-1000: жёлтый → оранжевый
/// - 1000-1500: оранжевый → красный (тут включается breathing pulse)
/// - 1500+: чистый красный
pub fn co2_to_rgb(co2: u16) -> Rgb {
    const MAX: u8 = 30;
    let m = MAX as f32;

    match co2 {
        0..=400 => Rgb::new(0, MAX, 0),
        // зелёный → жёлтый: красный канал поднимается 0 → MAX
        401..=700 => Rgb::new(lerp(0.0, m, segment_t(co2, 400, 700)), MAX, 0),
        // жёлтый → оранжевый: зелёный канал падает MAX → MAX/2
        701..=1000 => Rgb::new(MAX, lerp(m, m * 0.5, segment_t(co2, 700, 1000)), 0),
        // оранжевый → красный: зелёный канал падает MAX/2 → 0
        1001..=1500 => Rgb::new(MAX, lerp(m * 0.5, 0.0, segment_t(co2, 1000, 1500)), 0),
        _ => Rgb::new(MAX, 0, 0),
    }
}
