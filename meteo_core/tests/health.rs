use core::time::Duration;

use meteo_core::health::{
    Attach,
    Health,
    LAYOUT,
    NetStage,
    SNAPSHOT_WORDS,
    STALL_EXECUTOR,
    STALL_SENSOR,
    WORDS,
    Word,
};
use meteo_core::sensors::{
    Fault,
    Op,
    SensorId,
};
use meteo_core::supervisor::{
    BootFault,
    ResetKind,
    Uptime,
};
use portable_atomic::{
    AtomicU32,
    Ordering,
};

fn words() -> [AtomicU32; WORDS] {
    core::array::from_fn(|_| AtomicU32::new(0))
}

fn up(secs: u64) -> Uptime {
    Uptime(Duration::from_secs(secs))
}

#[test]
fn cold_garbage_is_wiped_and_warm_journal_continues() {
    let mem = words();
    for (i, w) in mem.iter().enumerate() {
        w.store(0xDEAD_0000 | u32::try_from(i).unwrap(), Ordering::Relaxed);
    }
    // Мусор без магии: даже не power-on обнуляет.
    let (health, attach) = Health::attach(&mem, false);
    assert_eq!(attach, Attach::Fresh);
    assert!(health.snapshot().0.iter().all(|w| *w == 0));

    health.bump(Word::Scd41Timeout);
    let (health, attach) = Health::attach(&mem, false);
    assert_eq!(attach, Attach::Continued, "software/WDT reset keeps the journal");
    assert_eq!(health.get(Word::Scd41Timeout), 1);

    let (health, attach) = Health::attach(&mem, true);
    assert_eq!(attach, Attach::Fresh, "power-on wipes even a valid journal");
    assert_eq!(health.get(Word::Scd41Timeout), 0);
}

#[test]
fn layout_is_stable_and_unique() {
    assert_eq!(LAYOUT, 1);
    let names: std::collections::HashSet<_> = Word::ALL.iter().map(|w| w.name()).collect();
    assert_eq!(names.len(), WORDS, "word names are unique");
    assert_eq!(Word::snapshot_fields().count(), SNAPSHOT_WORDS);
    assert_eq!(Word::snapshot_fields().next(), Some(Word::Generation));
    assert!(Word::snapshot_fields().all(|w| !w.name().starts_with("live.")));
}

#[test]
fn generation_tracks_transmitted_changes_only() {
    let mem = words();
    let (health, _) = Health::attach(&mem, true);
    let g0 = health.snapshot().generation();
    health.set(Word::SensorStage, 7);
    health.alive(up(10));
    assert_eq!(
        health.snapshot().generation(),
        g0,
        "live words do not bump the generation"
    );
    health.set(Word::WifiLastReason, 3);
    health.set(Word::WifiLastReason, 3);
    assert_eq!(
        health.snapshot().generation(),
        g0 + 1,
        "an unchanged value is not a change"
    );
    health.bump(Word::WifiDisconnects);
    assert_eq!(health.snapshot().generation(), g0 + 2);
}

#[test]
fn sensor_fault_records_the_last_fault_with_epoch() {
    let mem = words();
    let (health, _) = Health::attach(&mem, true);
    health.sensor_fault(SensorId::Scd41, Op::ScdRead.code(), Fault::Crc, up(5));
    assert_eq!(health.get(Word::Scd41Crc), 1);
    assert_eq!(health.get(Word::Scd41LastEpochS), 0, "clock not synced yet");

    // NTP: в uptime 100 с было 1 760 000 000 с epoch.
    health.clock_synced(up(100), 1_760_000_000_000);
    health.sensor_fault(SensorId::Scd41, Op::ScdDataReady.code(), Fault::Timeout, up(160));
    assert_eq!(health.get(Word::Scd41Timeout), 1);
    assert_eq!(health.get(Word::Scd41LastOp), Op::ScdDataReady.code().0);
    assert_eq!(health.get(Word::Scd41LastFault), Fault::Timeout.code());
    assert_eq!(health.get(Word::Scd41LastEpochS), 1_760_000_060);
}

/// Зависание, которое watchdog успел записать: место — его, причина — RWDT.
#[test]
fn recorded_stall_survives_the_watchdog_reset() {
    let mem = words();
    let (health, _) = Health::attach(&mem, true);
    assert_eq!(health.record_boot(ResetKind::PowerOn, 1), BootFault::Clean);
    health.clock_synced(up(10), 1_760_000_000_000);
    health.set(Word::SensorStage, Op::ScdDataReady.code().0);
    health.set(Word::NetStage, NetStage::Wait.code());
    health.record_stall(false, true, up(700));

    let (health, attach) = Health::attach(&mem, false);
    assert_eq!(attach, Attach::Continued);
    assert_eq!(health.record_boot(ResetKind::RtcWatchdog, 7), BootFault::WdtStall);
    assert_eq!(health.get(Word::BootWdt), 1);
    assert_eq!(health.get(Word::BootTotal), 2);
    assert_eq!(health.get(Word::StallTasks), STALL_SENSOR);
    assert_eq!(health.get(Word::StallSensorStage), Op::ScdDataReady.code().0);
    assert_eq!(health.get(Word::StallNetStage), NetStage::Wait.code());
    assert_eq!(health.get(Word::StallUptimeS), 700);
    assert_eq!(health.get(Word::StallEpochS), 1_759_999_990 + 700);
    assert_eq!(health.get(Word::ClockOffsetS), 0, "new boot starts unsynced");
    assert_eq!(health.faults_since_power_on(), 1);
}

/// RWDT без записи watchdog'а: встал весь executor; место — этапы и последний
/// тик прошлой загрузки.
#[test]
fn executor_stall_is_reconstructed_from_live_words() {
    let mem = words();
    let (health, _) = Health::attach(&mem, true);
    let _ = health.record_boot(ResetKind::PowerOn, 1);
    health.clock_synced(up(0), 1_760_000_000_000);
    health.set(Word::SensorStage, Op::BaroPoll.code().0);
    health.alive(up(300));

    let (health, _) = Health::attach(&mem, false);
    assert_eq!(health.record_boot(ResetKind::RtcWatchdog, 7), BootFault::WdtStall);
    assert_eq!(health.get(Word::StallTasks), STALL_EXECUTOR);
    assert_eq!(health.get(Word::StallSensorStage), Op::BaroPoll.code().0);
    assert_eq!(health.get(Word::StallUptimeS), 300);
    assert_eq!(health.get(Word::StallEpochS), 1_760_000_300);
}

#[test]
fn panic_mark_classifies_a_software_reset() {
    let mem = words();
    let (health, _) = Health::attach(&mem, true);
    let _ = health.record_boot(ResetKind::PowerOn, 1);
    health.mark_panic();
    let (health, _) = Health::attach(&mem, false);
    assert_eq!(health.record_boot(ResetKind::Software, 3), BootFault::Panic);
    assert_eq!(health.get(Word::BootPanic), 1);
    // Следующий штатный программный reset — уже не паника.
    let (health, _) = Health::attach(&mem, false);
    assert_eq!(health.record_boot(ResetKind::Software, 3), BootFault::Clean);
    assert_eq!(health.get(Word::BootPanic), 1);
}

#[test]
fn other_resets_are_counted_apart() {
    let mem = words();
    let (health, _) = Health::attach(&mem, true);
    let _ = health.record_boot(ResetKind::PowerOn, 1);
    let (health, _) = Health::attach(&mem, false);
    assert_eq!(health.record_boot(ResetKind::Other, 15), BootFault::Clean);
    assert_eq!(health.get(Word::BootOther), 1);
    assert_eq!(health.get(Word::BootLastReset), 15);
}

#[test]
fn changes_list_what_moved_since_the_previous_snapshot() {
    let mem = words();
    let (health, _) = Health::attach(&mem, true);
    let _ = health.record_boot(ResetKind::PowerOn, 1);
    let first = health.snapshot();
    let all: Vec<_> = first.changes(None).map(|(w, _, now)| (w, now)).collect();
    assert_eq!(all, vec![(Word::BootTotal, 1), (Word::BootLastReset, 1)]);

    health.bump(Word::Bmp390Bus);
    health.bump(Word::Bmp390Bus);
    let second = health.snapshot();
    let moved: Vec<_> = second.changes(Some(&first)).collect();
    assert_eq!(moved, vec![(Word::Bmp390Bus, 0, 2)]);
}
