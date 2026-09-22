use core::time::Duration;

use meteo_core::supervisor::{
    BootFault,
    Heartbeat,
    NET_STALL_LIMIT,
    ResetKind,
    SENSOR_STALL_LIMIT,
    Supervisor,
    Uptime,
    Verdict,
    classify,
};

fn at(secs: u64) -> Uptime {
    Uptime(Duration::from_secs(secs))
}

const SENSOR_STALLED: Verdict = Verdict::Withhold {
    sensor_ok: false,
    net_ok:    true,
};

#[test]
fn boot_grace_never_withholds_before_first_beat() {
    let mut sup = Supervisor::new(at(0), Heartbeat(0), Heartbeat(0));
    for t in (10..=3600).step_by(10) {
        assert_eq!(
            sup.tick(at(t), Heartbeat(0), Heartbeat(0)),
            Verdict::Feed,
            "t={t}"
        );
    }
}

#[test]
fn sensor_stall_withholds_exactly_at_limit() {
    let mut sup = Supervisor::new(at(0), Heartbeat(0), Heartbeat(0));
    // Первый heartbeat в 10с — таска взведена.
    assert_eq!(sup.tick(at(10), Heartbeat(1), Heartbeat(0)), Verdict::Feed);
    let limit = SENSOR_STALL_LIMIT.as_secs();
    assert_eq!(
        sup.tick(at(10 + limit - 1), Heartbeat(1), Heartbeat(0)),
        Verdict::Feed
    );
    assert_eq!(
        sup.tick(at(10 + limit), Heartbeat(1), Heartbeat(0)),
        SENSOR_STALLED
    );
    assert_eq!(
        sup.tick(at(10 + limit + 60), Heartbeat(1), Heartbeat(0)),
        SENSOR_STALLED
    );
}

#[test]
fn net_stall_uses_its_own_limit() {
    let mut sup = Supervisor::new(at(0), Heartbeat(0), Heartbeat(0));
    assert_eq!(sup.tick(at(5), Heartbeat(0), Heartbeat(1)), Verdict::Feed);
    let limit = NET_STALL_LIMIT.as_secs();
    // Sensor ещё в boot-grace, net молчит дольше sensor-лимита — это легитимно.
    assert_eq!(
        sup.tick(at(5 + limit - 1), Heartbeat(0), Heartbeat(1)),
        Verdict::Feed
    );
    assert_eq!(
        sup.tick(at(5 + limit), Heartbeat(0), Heartbeat(1)),
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
    let mut sup = Supervisor::new(at(0), Heartbeat(0), Heartbeat(0));
    sup.tick(at(10), Heartbeat(1), Heartbeat(1));
    let stalled_at = 10 + SENSOR_STALL_LIMIT.as_secs();
    assert_eq!(
        sup.tick(at(stalled_at), Heartbeat(1), Heartbeat(2)),
        SENSOR_STALLED
    );
    assert_eq!(
        sup.tick(at(stalled_at + 10), Heartbeat(2), Heartbeat(3)),
        Verdict::Feed
    );
    // И лимит отсчитывается от оживления, а не от первого boot'а.
    let again = stalled_at + 10 + SENSOR_STALL_LIMIT.as_secs();
    assert_eq!(sup.tick(at(again - 1), Heartbeat(2), Heartbeat(4)), Verdict::Feed);
    assert_eq!(sup.tick(at(again), Heartbeat(2), Heartbeat(5)), SENSOR_STALLED);
}

#[test]
fn heartbeat_wraparound_still_counts_as_life() {
    let mut sup = Supervisor::new(at(0), Heartbeat(u32::MAX), Heartbeat(0));
    assert_eq!(sup.tick(at(10), Heartbeat(0), Heartbeat(0)), Verdict::Feed);
    let limit = SENSOR_STALL_LIMIT.as_secs();
    assert_eq!(
        sup.tick(at(10 + limit), Heartbeat(0), Heartbeat(0)),
        SENSOR_STALLED
    );
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
