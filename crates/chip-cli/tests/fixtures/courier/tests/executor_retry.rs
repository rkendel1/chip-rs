//! How a request is retried: how many attempts it gets, how long it waits, and what ends it.

mod common;

use common::rig;
use courier::transport::mock::{connect_error, ok, retry_after, status};
use courier::ErrorKind;
use std::time::Duration;

#[test]
fn a_transient_failure_is_retried_with_exponential_backoff() {
    let r = rig("", vec![status(503), status(502), ok()]);
    assert!(r.client.get("http://h/a").is_ok());
    assert_eq!(r.transport.calls(), 3);
    assert_eq!(
        r.clock.slept(),
        vec![Duration::from_millis(100), Duration::from_millis(200)]
    );
}

#[test]
fn three_attempts_is_the_limit_by_default() {
    let r = rig("", vec![status(500); 6]);
    let e = r.client.get("http://h/a").unwrap_err();
    assert_eq!(e.kind, ErrorKind::RetriesExhausted);
    assert_eq!(e.attempts, 3);
    assert_eq!(r.transport.calls(), 3);
}

#[test]
fn terminal_failures_are_not_retried() {
    for code in [400, 401, 404, 422] {
        let r = rig("", vec![status(code), ok()]);
        let e = r.client.get("http://h/a").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Status, "{code}");
        assert_eq!(r.transport.calls(), 1, "{code}");
    }
}

#[test]
fn connection_failures_are_retried() {
    let r = rig("", vec![connect_error(), connect_error(), ok()]);
    assert!(r.client.get("http://h/a").is_ok());
    assert_eq!(r.transport.calls(), 3);
}

#[test]
fn retry_after_longer_than_the_backoff_is_honored() {
    let r = rig("", vec![retry_after(503, 90), ok()]);
    assert!(r.client.get("http://h/a").is_ok());
    assert_eq!(r.clock.slept(), vec![Duration::from_secs(90)]);
}

#[test]
fn retry_after_shorter_than_the_backoff_does_not_shorten_it() {
    let r = rig("", vec![status(503), status(503), retry_after(503, 0), ok()]);
    // The third wait would be 400ms of backoff; the hint of 0s must not shorten it.
    let _ = r.client.get("http://h/a");
    assert!(r.clock.slept().iter().all(|d| *d >= Duration::from_millis(100)));
}

#[test]
fn retry_after_responses_spend_the_attempt_budget() {
    // A server that keeps answering 429 with a hint is still being asked again each time, so the
    // request must stop after the same three attempts as any other persistent failure.
    let r = rig("", vec![retry_after(429, 1); 8]);
    let e = r.client.get("http://h/a").unwrap_err();
    assert_eq!(e.kind, ErrorKind::RetriesExhausted);
    assert_eq!(e.attempts, 3);
    assert_eq!(r.transport.calls(), 3);
    assert_eq!(r.clock.slept(), vec![Duration::from_secs(1); 2]);
}

#[test]
fn a_deadline_ends_the_request_before_a_long_wait() {
    let r = rig("[client]\ndeadline = 20s\n", vec![retry_after(503, 90), ok()]);
    let e = r.client.get("http://h/a").unwrap_err();
    assert_eq!(e.kind, ErrorKind::DeadlineExceeded);
    assert!(r.clock.slept().is_empty());
    assert_eq!(r.transport.calls(), 1);
}

#[test]
fn every_attempt_uses_the_configured_timeout() {
    let r = rig("[client]\ntimeout = 3s\n", vec![status(503), ok()]);
    r.client.get("http://h/a").unwrap();
    assert_eq!(r.transport.timeouts(), vec![Duration::from_secs(3); 2]);
}

#[test]
fn post_requests_are_retried_like_any_other() {
    let r = rig("", vec![status(503), ok()]);
    let req = courier::Request::post("http://h/a", b"payload").unwrap();
    assert!(r.client.send(req).is_ok());
    assert_eq!(r.transport.requests()[1].body, b"payload");
}
