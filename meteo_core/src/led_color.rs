//! CO2 → цвет и «дыхание» индикатора. Чистые функции без LEDC: прошивка
//! только применяет результат к каналам.

use crate::codec::Ppm;

/// Яркость канала в процентах (0..=100) — ровно то, что принимает LEDC
/// `set_duty`.
///
/// Держим триплет одной структурой: три параллельных `u8`
/// (`cur_r`/`cur_g`/`cur_b` плюс кортежи в таблицах цветов) слишком легко
/// разъезжаются и путаются местами.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const OFF: Self = Self::new(0, 0, 0);

    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Масштабирует яркость на `pct` процентов (0 = выключено, 100 = без
    /// изменений). Значения выше 100 ограничиваются до 100, поэтому
    /// результат не ярче исходного канала. Компоненты `Rgb` выше 100 остаются
    /// выше 100 и при масштабировании на 100.
    ///
    /// # Panics
    ///
    /// This function cannot panic: after clamping `pct`, the scaled channel
    /// value is no greater than its original `u8` value.
    #[must_use]
    pub fn scaled(self, pct: u8) -> Self {
        let pct = pct.min(100);
        let s = |c: u8| {
            u8::try_from((u16::from(c) * u16::from(pct)) / 100).expect("scaled brightness stays within u8")
        };
        Self::new(s(self.r), s(self.g), s(self.b))
    }
}

/// Предельная яркость CO2-палитры, проценты. Ниже blink-кодов: цвет воздуха
/// горит постоянно и не должен слепить.
pub const CO2_MAX_LEVEL: u8 = 30;

/// Линейная интерполяция в u8: a→b по t∈[0,1].
fn lerp(a: f32, b: f32, t: f32) -> u8 {
    let value = a + (b - a) * t;
    debug_assert!((0.0..=f32::from(u8::MAX)).contains(&value));
    // All call sites pass a t in [0, 1] and endpoints in the u8 range.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        value as u8
    }
}

/// Параметр t∈[0,1] для co2 в отрезке [lo, hi]. Предусловие: lo ≤ co2 ≤ hi.
fn segment_t(co2: Ppm, lo: u16, hi: u16) -> f32 {
    f32::from(co2.0 - lo) / f32::from(hi - lo)
}

/// CO2 ppm → цвет, яркость в долях `CO2_MAX_LEVEL`.
///
/// Детализированная зона 400-700 — основная зона мониторинга проветривания.
/// - 0-400: чистый зелёный
/// - 400-700: зелёный → жёлто-зелёный → жёлтый (мягкий gradient для тонкой оценки)
/// - 700-1000: жёлтый → оранжевый
/// - 1000-1500: оранжевый → красный (тут включается breathing pulse)
/// - 1500+: чистый красный
#[must_use]
pub fn co2_to_rgb(co2: Ppm) -> Rgb {
    const MAX: u8 = CO2_MAX_LEVEL;
    let m = f32::from(MAX);

    match co2.0 {
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

/// Параметры breathing-пульса для повышенного CO2.
/// `period_ms` — длительность полного цикла dim→bright→dim.
/// `depth_pct` — насколько затемняется LED в нижней точке (0..100, где 100 = до полного off).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PulseSpec {
    pub period_ms: u16,
    pub depth_pct: u8,
}

/// CO2 → параметры пульса. `None` = статичный цвет (нормальная атмосфера).
/// Чем выше CO2, тем быстрее и глубже дыхание.
#[must_use]
pub const fn pulse_params(co2: Ppm) -> Option<PulseSpec> {
    match co2.0 {
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
