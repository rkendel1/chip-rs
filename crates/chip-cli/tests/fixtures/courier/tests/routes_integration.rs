//! Routes: which settings a request gets, and when they are decided.

mod common;

use common::rig;
use courier::routes::RouteOverrides;
use courier::transport::mock::{ok, status};
use courier::ErrorKind;
use std::time::Duration;

const CONFIG: &str = "[client]\ntimeout = 4s\nmax_wait = 8s\n\n[route.reports]\nprefix = /reports\ntimeout = 30s\n\n[route.fast]\nprefix = /fast\nmax_inflight = 2\n";

#[test]
fn a_route_overrides_only_what_it_sets() {
    let r = rig(CONFIG, vec![ok(), ok(), ok()]);
    r.client.get("http://h/reports/q1").unwrap();
    r.client.get("http://h/fast/x").unwrap();
    r.client.get("http://h/other").unwrap();
    assert_eq!(
        r.transport.timeouts(),
        vec![Duration::from_secs(30), Duration::from_secs(4), Duration::from_secs(4)]
    );
}

#[test]
fn a_route_keeps_the_client_wait_limit() {
    let r = rig(CONFIG, vec![courier::transport::mock::retry_after(503, 60), ok()]);
    let e = r.client.get("http://h/reports/q1").unwrap_err();
    assert_eq!(e.kind, ErrorKind::WaitTooLong);
}

#[test]
fn a_route_may_replace_the_wait_limit() {
    let cfg = "[client]\nmax_wait = 8s\n[route.slow]\nprefix = /slow\nmax_wait = 120s\n";
    let r = rig(cfg, vec![courier::transport::mock::retry_after(503, 60), ok()]);
    assert!(r.client.get("http://h/slow/x").is_ok());
}

#[test]
fn overrides_added_after_construction_apply_to_the_next_request() {
    let r = rig(CONFIG, vec![ok(), ok()]);
    r.client.get("http://h/reports/a").unwrap();
    assert!(r.client.set_route_overrides(
        "reports",
        RouteOverrides {
            timeout: Some(Duration::from_secs(99)),
            ..Default::default()
        }
    ));
    r.client.get("http://h/reports/b").unwrap();
    assert_eq!(r.transport.timeouts()[1], Duration::from_secs(99));
}

#[test]
fn the_route_is_reported_for_each_request() {
    let r = rig(CONFIG, vec![]);
    let url = courier::Url::parse("http://h/reports/q1").unwrap();
    assert_eq!(r.client.route_for(&url).route, "reports");
    let other = courier::Url::parse("http://h/elsewhere").unwrap();
    assert_eq!(r.client.route_for(&other).route, "default");
}

#[test]
fn failures_on_one_route_do_not_open_another() {
    let cfg = "[circuit]\nthreshold = 1\n[route.a]\nprefix = /a\n[route.b]\nprefix = /b\n";
    let r = rig(cfg, vec![status(404), status(500), status(500), status(500), ok()]);
    let _ = r.client.get("http://h/a/1");
    assert!(r.client.get("http://h/b/1").is_err());
    assert_eq!(r.client.get("http://h/b/2").unwrap_err().kind, ErrorKind::CircuitOpen);
    assert!(r.client.get("http://h/a/2").is_ok(), "route a is unaffected by route b");
}
