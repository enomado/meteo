use core::time::Duration;

use meteo_core::wifi_retry::{
    RETRY_PAUSE,
    ReasonCode,
    Reconnect,
    Verdict,
};

const OTHER: ReasonCode = ReasonCode(15); // 4-way handshake timeout

const IMMEDIATE_QUIET: Verdict = Verdict {
    pause:        Duration::ZERO,
    raise_status: false,
};
const PAUSED_QUIET: Verdict = Verdict {
    pause:        RETRY_PAUSE,
    raise_status: false,
};
const PAUSED_RAISED: Verdict = Verdict {
    pause:        RETRY_PAUSE,
    raise_status: true,
};

#[test]
fn stale_session_after_boot_is_retried_at_once_without_code() {
    let mut r = Reconnect::new();
    assert_eq!(r.on_failed(Some(ReasonCode::AUTH_EXPIRED)), IMMEDIATE_QUIET);
}

#[test]
fn single_failure_keeps_the_pause_but_not_the_code() {
    let mut r = Reconnect::new();
    assert_eq!(r.on_failed(Some(OTHER)), PAUSED_QUIET);
    let mut r = Reconnect::new();
    assert_eq!(r.on_failed(None), PAUSED_QUIET);
}

#[test]
fn second_failure_in_a_row_raises_the_code() {
    let mut r = Reconnect::new();
    r.on_failed(Some(ReasonCode::AUTH_EXPIRED));
    // Повторный AUTH_EXPIRED — уже не остаток прошлой сессии: обычная пауза.
    assert_eq!(r.on_failed(Some(ReasonCode::AUTH_EXPIRED)), PAUSED_RAISED);
    assert_eq!(r.on_failed(Some(OTHER)), PAUSED_RAISED);
}

#[test]
fn connection_and_drop_restart_the_count() {
    let mut r = Reconnect::new();
    r.on_failed(Some(OTHER));
    r.on_connected();
    assert_eq!(r.on_failed(Some(ReasonCode::AUTH_EXPIRED)), IMMEDIATE_QUIET);

    let mut r = Reconnect::new();
    r.on_failed(Some(OTHER));
    r.on_dropped();
    assert_eq!(r.on_failed(Some(OTHER)), PAUSED_QUIET);
}
