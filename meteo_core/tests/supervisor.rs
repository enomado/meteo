use core::time::Duration;

use meteo_core::supervisor::{
    BootFault,
    Heartbeat,
    NET_STALL_LIMIT,
    ResetKind,
    SENSOR_STALL_LIMIT,
    Supervisor,
    TaskHeartbeats,
    Uptime,
    Verdict,
    classify,
};

fn at(secs: u64) -> Uptime {
    Uptime(Duration::from_secs(secs))
}

fn heartbeats(sensor: u32, network: u32) -> TaskHeartbeats {
    TaskHeartbeats {
        sensor:  Heartbeat(sensor),
        network: Heartbeat(network),
    }
}

const SENSOR_STALLED: Verdict = Verdict::Withhold {
    sensor_ok: false,
    net_ok:    true,
};

#[test]
fn boot_grace_never_withholds_before_first_beat() {
    let mut sup = Supervisor::new(at(0), heartbeats(0, 0));
    for t in (10..=3600).step_by(10) {
        assert_eq!(sup.tick(at(t), heartbeats(0, 0)), Verdict::Feed, "t={t}");
    }
}

#[test]
fn sensor_stall_withholds_exactly_at_limit() {
    let mut sup = Supervisor::new(at(0), heartbeats(0, 0));
    // Первый heartbeat в 10с — таска взведена.
    assert_eq!(sup.tick(at(10), heartbeats(1, 0)), Verdict::Feed);
    let limit = SENSOR_STALL_LIMIT.as_secs();
    assert_eq!(sup.tick(at(10 + limit - 1), heartbeats(1, 0)), Verdict::Feed);
    assert_eq!(sup.tick(at(10 + limit), heartbeats(1, 0)), SENSOR_STALLED);
    assert_eq!(sup.tick(at(10 + limit + 60), heartbeats(1, 0)), SENSOR_STALLED);
}

#[test]
fn net_stall_uses_its_own_limit() {
    let mut sup = Supervisor::new(at(0), heartbeats(0, 0));
    assert_eq!(sup.tick(at(5), heartbeats(0, 1)), Verdict::Feed);
    let limit = NET_STALL_LIMIT.as_secs();
    // Sensor ещё в boot-grace, net молчит дольше sensor-лимита — это легитимно.
    assert_eq!(sup.tick(at(5 + limit - 1), heartbeats(0, 1)), Verdict::Feed);
    assert_eq!(
        sup.tick(at(5 + limit), heartbeats(0, 1)),
        Verdict::Withhold {
            sensor_ok: true,
            net_ok:    false,
        }
    );
}

/// Таска ожила раньше аппаратного таймаута — надзор снова кормит, reset
/// отменяется (раньше маркер столла при этом уже был записан и не снимался).
#[test]
fn revival_before_hw_timeout_resumes_feeding() {
    let mut sup = Supervisor::new(at(0), heartbeats(0, 0));
    sup.tick(at(10), heartbeats(1, 1));
    let stalled_at = 10 + SENSOR_STALL_LIMIT.as_secs();
    assert_eq!(sup.tick(at(stalled_at), heartbeats(1, 2)), SENSOR_STALLED);
    assert_eq!(sup.tick(at(stalled_at + 10), heartbeats(2, 3)), Verdict::Feed);
    // И лимит отсчитывается от оживления, а не от первого boot'а.
    let again = stalled_at + 10 + SENSOR_STALL_LIMIT.as_secs();
    assert_eq!(sup.tick(at(again - 1), heartbeats(2, 4)), Verdict::Feed);
    assert_eq!(sup.tick(at(again), heartbeats(2, 5)), SENSOR_STALLED);
}

#[test]
fn heartbeat_wraparound_still_counts_as_life() {
    let mut sup = Supervisor::new(at(0), heartbeats(u32::MAX, 0));
    assert_eq!(sup.tick(at(10), heartbeats(0, 0)), Verdict::Feed);
    let limit = SENSOR_STALL_LIMIT.as_secs();
    assert_eq!(sup.tick(at(10 + limit), heartbeats(0, 0)), SENSOR_STALLED);
}

#[test]
fn boot_fault_classification() {
    for marked in [false, true] {
        assert_eq!(classify(ResetKind::RtcWatchdog, marked), BootFault::WdtStall);
        assert_eq!(classify(ResetKind::PowerOn, marked), BootFault::Clean);
        assert_eq!(classify(ResetKind::Other, marked), BootFault::Clean);
    }
    assert_eq!(classify(ResetKind::Software, true), BootFault::Panic);
    assert_eq!(classify(ResetKind::Software, false), BootFault::Clean);
}
