//! The circuit breaker as the client drives it.

mod common;

use common::{rig, scratch};
use courier::middleware::circuit::State;
use courier::transport::mock::{ok, status};
use courier::ErrorKind;
use std::time::Duration;

#[test]
fn the_breaker_counts_requests_not_attempts() {
    // Each request below makes three attempts but is a single failure.
    let r = rig("[circuit]\nthreshold = 2\n", vec![status(503); 9]);
    assert_eq!(r.client.get("http://h/a").unwrap_err().kind, ErrorKind::RetriesExhausted);
    assert_eq!(r.client.breaker_state("default"), State::Closed);
    assert_eq!(r.client.get("http://h/a").unwrap_err().kind, ErrorKind::RetriesExhausted);
    assert_eq!(r.client.breaker_state("default"), State::Open);
    assert_eq!(r.transport.calls(), 6);
}

#[test]
fn an_open_circuit_rejects_without_touching_the_transport() {
    let r = rig("[circuit]\nthreshold = 1\n", vec![status(503); 3]);
    let _ = r.client.get("http://h/a");
    let e = r.client.get("http://h/a").unwrap_err();
    assert_eq!(e.kind, ErrorKind::CircuitOpen);
    assert_eq!(r.transport.calls(), 3);
}

#[test]
fn the_circuit_recovers_after_the_cooldown() {
    let r = rig("[circuit]\nthreshold = 1\ncooldown = 10s\n", vec![status(503), status(503), status(503), ok()]);
    let _ = r.client.get("http://h/a");
    r.clock.advance(Duration::from_secs(10));
    assert!(r.client.get("http://h/a").is_ok());
    assert_eq!(r.client.breaker_state("default"), State::Closed);
}

#[test]
fn client_errors_do_not_count_against_the_circuit() {
    let r = rig("[circuit]\nthreshold = 1\n", vec![status(404), status(404), ok()]);
    assert!(r.client.get("http://h/a").is_err());
    assert!(r.client.get("http://h/a").is_err());
    assert!(r.client.get("http://h/a").is_ok());
}

#[test]
fn a_disabled_circuit_never_opens() {
    let r = rig("[circuit]\nenabled = false\nthreshold = 1\n", vec![status(503); 6]);
    let _ = r.client.get("http://h/a");
    let e = r.client.get("http://h/a").unwrap_err();
    assert_eq!(e.kind, ErrorKind::RetriesExhausted);
}

#[test]
fn an_open_circuit_survives_a_restart() {
    let dir = scratch("circuit-restart");
    let journal = dir.join("journal.log");
    let cfg = format!(
        "[circuit]\nthreshold = 1\ncooldown = 1h\n[journal]\nenabled = true\npath = {}\n",
        journal.display()
    );
    let first = rig(&cfg, vec![status(503); 3]);
    let _ = first.client.get("http://h/a");
    assert_eq!(first.client.breaker_state("default"), State::Open);
    drop(first);

    let second = rig(&cfg, vec![ok()]);
    assert_eq!(second.client.breaker_state("default"), State::Open);
    assert_eq!(second.client.get("http://h/a").unwrap_err().kind, ErrorKind::CircuitOpen);
}
