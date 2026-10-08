//! `max_wait`: a wait longer than the limit fails the request instead of sleeping.

mod common;

use common::rig;
use courier::transport::mock::{ok, retry_after, status};
use courier::ErrorKind;
use std::time::Duration;

#[test]
fn a_server_hint_beyond_max_wait_fails_the_request() {
    let r = rig("[client]\nmax_wait = 10s\n", vec![retry_after(429, 90), ok()]);
    let e = r.client.get("http://h/a").unwrap_err();
    assert_eq!(e.kind, ErrorKind::WaitTooLong);
    assert!(r.clock.slept().is_empty());
    assert_eq!(r.transport.calls(), 1);
}

#[test]
fn a_server_hint_within_max_wait_is_honored() {
    let r = rig("[client]\nmax_wait = 120s\n", vec![retry_after(429, 90), ok()]);
    assert!(r.client.get("http://h/a").is_ok());
    assert_eq!(r.clock.slept(), vec![Duration::from_secs(90)]);
}

#[test]
fn without_max_wait_any_hint_is_honored() {
    let r = rig("", vec![retry_after(503, 3600), ok()]);
    assert!(r.client.get("http://h/a").is_ok());
    assert_eq!(r.clock.total_slept(), Duration::from_secs(3600));
}

#[test]
fn backoff_waits_are_checked_against_max_wait_too() {
    let r = rig("[client]\nmax_wait = 150ms\n", vec![status(503), status(503), ok()]);
    let e = r.client.get("http://h/a").unwrap_err();
    assert_eq!(e.kind, ErrorKind::WaitTooLong);
    assert_eq!(e.attempts, 2);
}

#[test]
fn the_environment_can_set_max_wait() {
    let r = common::rig_with_env("", &[("COURIER_CLIENT_MAX_WAIT", "5s")], vec![retry_after(429, 9), ok()]);
    assert_eq!(r.client.get("http://h/a").unwrap_err().kind, ErrorKind::WaitTooLong);
}
