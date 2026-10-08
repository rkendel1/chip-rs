mod common;

use common::rig;
use courier::transport::mock::ok;
use courier::ErrorKind;
use std::time::Duration;

const CFG: &str = "[rate_limit]\nenabled = true\nrate = 2\nburst = 2\n";

#[test]
fn requests_beyond_the_burst_are_rejected() {
    let r = rig(CFG, vec![ok(), ok(), ok()]);
    assert!(r.client.get("http://h/a").is_ok());
    assert!(r.client.get("http://h/a").is_ok());
    let e = r.client.get("http://h/a").unwrap_err();
    assert_eq!(e.kind, ErrorKind::RateLimited);
    assert_eq!(r.transport.calls(), 2);
}

#[test]
fn tokens_come_back_with_time() {
    let r = rig(CFG, vec![ok(), ok(), ok()]);
    r.client.get("http://h/a").unwrap();
    r.client.get("http://h/a").unwrap();
    r.clock.advance(Duration::from_secs(1));
    assert!(r.client.get("http://h/a").is_ok());
}

#[test]
fn a_rejection_is_counted() {
    let r = rig(CFG, vec![ok(), ok()]);
    for _ in 0..4 {
        let _ = r.client.get("http://h/a");
    }
    assert_eq!(r.client.metrics().value("rejected"), 2);
}
