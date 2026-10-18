use core::time::Duration;

use meteo_core::backlog::Unsynced;
use meteo_core::codec::{
    CentiPascal,
    EpochMillis,
    MilliCelsius,
    Ppm,
};
use meteo_core::sensors::Sample;
use meteo_core::supervisor::Uptime;

/// Epoch момента boot, который «вернул» NTP: 2026-10-18 12:00:00 UTC.
const BOOT_EPOCH_MS: u64 = 1_792_324_800_000;

fn up(secs: u64) -> Uptime {
    Uptime(Duration::from_secs(secs))
}

fn epoch_at(at: Uptime) -> EpochMillis {
    EpochMillis(BOOT_EPOCH_MS + u64::try_from(at.0.as_millis()).unwrap())
}

fn sample(co2: u16) -> Sample {
    Sample {
        co2: Some(Ppm(co2)),
        pressure: Some(CentiPascal(10_132_500)),
        ..Sample::default()
    }
}

#[test]
fn held_readings_get_the_epoch_of_their_own_cycle_oldest_first() {
    let mut unsynced = Unsynced::new();
    assert_eq!(unsynced.hold(up(8), sample(410)), None);
    assert_eq!(unsynced.hold(up(38), sample(420)), None);

    let first = unsynced.release(epoch_at).expect("two readings are held");
    let second = unsynced.release(epoch_at).expect("two readings are held");
    assert_eq!(unsynced.release(epoch_at), None);

    // Время — момент цикла (uptime + смещение), а не момент выдачи.
    assert_eq!(
        first,
        sample(410).into_reading(EpochMillis(BOOT_EPOCH_MS + 8_000))
    );
    assert_eq!(
        second,
        sample(420).into_reading(EpochMillis(BOOT_EPOCH_MS + 38_000))
    );
}

#[test]
fn overflow_evicts_the_oldest_and_reports_its_moment() {
    let mut unsynced = Unsynced::new();
    // Ёмкость — 59 показаний (heapless `Queue<_, 60>`).
    for i in 0..59 {
        assert_eq!(unsynced.hold(up(i * 30), sample(400)), None, "cycle {i} fits");
    }
    assert_eq!(unsynced.hold(up(59 * 30), sample(400)), Some(up(0)));

    let first = unsynced.release(epoch_at).expect("the queue is full");
    assert_eq!(first.time, epoch_at(up(30)));
}

#[test]
fn a_sample_without_channels_is_empty() {
    assert!(!Sample::default().has_any_channel());
    assert!(
        Sample {
            scd_temp: Some(MilliCelsius(18_200)),
            ..Sample::default()
        }
        .has_any_channel()
    );
    assert!(sample(400).has_any_channel());
}
