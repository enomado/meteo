//! CO2 → цвет и «дыхание» индикатора. Чистые функции без LEDC: прошивка
//! только применяет результат к каналам.

/// Яркость канала в процентах (0..=100) — ровно то, что принимает LEDC
/// `set_duty`. Держим триплет одной структурой: три параллельных `u8`
/// (cur_r/cur_g/cur_b плюс кортежи в таблицах цветов) слишком легко разъезжаются
/// и путаются местами.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    pub fn scaled(self, pct: u8) -> Self {
        let s = |c: u8| ((c as u16 * pct as u16) / 100) as u8;
        Self::new(s(self.r), s(self.g), s(self.b))
    }
}

/// Предельная яркость CO2-палитры, проценты. Ниже blink-кодов: цвет воздуха
/// горит постоянно и не должен слепить.
pub const CO2_MAX_LEVEL: u8 = 30;

/// Линейная интерполяция в u8: a→b по t∈[0,1].
fn lerp(a: f32, b: f32, t: f32) -> u8 {
    (a + (b - a) * t) as u8
}

/// Параметр t∈[0,1] для co2 в отрезке [lo, hi]. Предусловие: lo ≤ co2 ≤ hi.
fn segment_t(co2: u16, lo: u16, hi: u16) -> f32 {
    ((co2 - lo) as f32) / ((hi - lo) as f32)
}

/// CO2 ppm → цвет, яркость в долях `CO2_MAX_LEVEL`.
///
/// Детализированная зона 400-700 — основная зона мониторинга проветривания.
/// - 0-400: чистый зелёный
/// - 400-700: зелёный → жёлто-зелёный → жёлтый (мягкий gradient для тонкой оценки)
/// - 700-1000: жёлтый → оранжевый
/// - 1000-1500: оранжевый → красный (тут включается breathing pulse)
/// - 1500+: чистый красный
pub fn co2_to_rgb(co2: u16) -> Rgb {
    const MAX: u8 = CO2_MAX_LEVEL;
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
pub fn pulse_params(co2: u16) -> Option<PulseSpec> {
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
