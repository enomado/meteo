//! Полный перебор `u16`: 65 536 входов — это доказательство, а не выборка.

use meteo_core::led_color::{
    CO2_MAX_LEVEL,
    PulseSpec,
    Rgb,
    co2_to_rgb,
    pulse_params,
};

fn all_co2() -> impl Iterator<Item = u16> {
    0..=u16::MAX
}

#[test]
fn co2_colour_stays_in_palette() {
    for co2 in all_co2() {
        let c = co2_to_rgb(co2);
        assert!(c.r <= CO2_MAX_LEVEL && c.g <= CO2_MAX_LEVEL, "co2={co2}: {c:?}");
        assert_eq!(c.b, 0, "co2={co2}: CO2 palette has no blue");
    }
    assert_eq!(co2_to_rgb(0), Rgb::new(0, CO2_MAX_LEVEL, 0));
    assert_eq!(co2_to_rgb(1500), Rgb::new(CO2_MAX_LEVEL, 0, 0));
    assert_eq!(co2_to_rgb(u16::MAX), Rgb::new(CO2_MAX_LEVEL, 0, 0));
}

/// Нет скачков на стыках сегментов: соседние ppm различаются не больше чем на
/// единицу яркости в каждом канале. Красный только растёт, зелёный только
/// падает — цвет движется green → yellow → orange → red без возвратов.
#[test]
fn co2_colour_is_continuous_and_monotonic() {
    let mut prev = co2_to_rgb(0);
    for co2 in 1..=u16::MAX {
        let cur = co2_to_rgb(co2);
        assert!(
            cur.r.abs_diff(prev.r) <= 1,
            "red jump at {co2}: {prev:?} -> {cur:?}"
        );
        assert!(
            cur.g.abs_diff(prev.g) <= 1,
            "green jump at {co2}: {prev:?} -> {cur:?}"
        );
        assert!(cur.r >= prev.r, "red decreased at {co2}");
        assert!(cur.g <= prev.g, "green increased at {co2}");
        prev = cur;
    }
}

/// Пульс включается с 1000 ppm и с ростом CO2 только ускоряется и углубляется.
/// `depth_pct ≤ 100` — предусловие `100 - depth_pct` в прошивке; полупериод
/// ненулевой — иначе fade за 0 мс.
#[test]
fn pulse_escalates_monotonically() {
    let mut prev: Option<PulseSpec> = None;
    for co2 in all_co2() {
        let p = pulse_params(co2);
        assert_eq!(p.is_some(), co2 >= 1000, "co2={co2}");
        if let Some(spec) = p {
            assert!(spec.depth_pct <= 100, "co2={co2}: {spec:?}");
            assert!(spec.period_ms / 2 > 0, "co2={co2}: {spec:?}");
            if let Some(before) = prev {
                assert!(spec.period_ms <= before.period_ms, "slower pulse at {co2}");
                assert!(spec.depth_pct >= before.depth_pct, "shallower pulse at {co2}");
            }
        }
        prev = p;
    }
}

#[test]
fn scaled_never_brightens() {
    for c in 0..=u8::MAX {
        let rgb = Rgb::new(c, c, c);
        assert_eq!(rgb.scaled(100), rgb);
        assert_eq!(rgb.scaled(0), Rgb::OFF);
        for pct in 0..=100 {
            assert!(rgb.scaled(pct).r <= c, "c={c} pct={pct}");
        }
    }
}
