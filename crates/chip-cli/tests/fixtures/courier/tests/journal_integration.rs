//! What the journal records for finished requests.

mod common;

use common::{rig, scratch};
use courier::state::journal::Outcome;
use courier::transport::mock::{ok, status};

fn config(dir: &std::path::Path) -> String {
    format!("[journal]\nenabled = true\npath = {}\n", dir.join("j.log").display())
}

#[test]
fn a_successful_request_is_journalled() {
    let dir = scratch("journal-ok");
    let r = rig(&config(&dir), vec![ok()]);
    r.client.get("http://h/a").unwrap();
    let read = r.client.journal().unwrap().read().unwrap();
    assert_eq!(read.entries.len(), 1);
    assert_eq!(read.entries[0].outcome, Outcome::Success);
    assert_eq!(read.entries[0].url, "http://h/a");
}

#[test]
fn a_failed_request_records_how_many_attempts_it_took() {
    let dir = scratch("journal-fail");
    let r = rig(&config(&dir), vec![status(503); 3]);
    let _ = r.client.get("http://h/a");
    let e = &r.client.journal().unwrap().read().unwrap().entries[0];
    assert_eq!(e.outcome, Outcome::Failure);
    assert_eq!(e.attempts, 3);
    assert!(e.detail.contains("retries_exhausted"));
}

#[test]
fn rejected_requests_are_journalled_with_zero_attempts() {
    let dir = scratch("journal-rejected");
    let cfg = format!("{}\n[circuit]\nthreshold = 1\n", config(&dir));
    let r = rig(&cfg, vec![status(503); 3]);
    let _ = r.client.get("http://h/a");
    let _ = r.client.get("http://h/a");
    let entries = r.client.journal().unwrap().read().unwrap().entries;
    assert_eq!(entries[1].outcome, Outcome::Rejected);
    assert_eq!(entries[1].attempts, 0);
}

#[test]
fn without_a_journal_nothing_is_written() {
    let r = rig("", vec![ok()]);
    r.client.get("http://h/a").unwrap();
    assert!(r.client.journal().is_none());
}

#[test]
fn metrics_agree_with_the_journal() {
    let dir = scratch("journal-metrics");
    let r = rig(&config(&dir), vec![ok(), status(404)]);
    let _ = r.client.get("http://h/a");
    let _ = r.client.get("http://h/b");
    let m = r.client.metrics();
    assert_eq!(m.value("requests"), 2);
    assert_eq!(m.value("successes"), 1);
    assert_eq!(m.value("failures"), 1);
    assert_eq!(r.client.journal().unwrap().read().unwrap().entries.len(), 2);
}
