//! Phase B: the smallest proof that Chip's build can drive the real, native Rust FeltDB.
//!
//! No mock, no service, no runtime beyond what the `feltdb` crate itself links. FeltDB's Rust core
//! has no in-memory backend (see the compatibility report), so the smallest supported mode is its
//! file journal in a temporary directory; the filesystem and cleanup implications are asserted
//! below rather than assumed.

use feltdb::{FeltDb, FlowError};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Note {
    session: String,
    status: String,
    n: u32,
}

fn note(session: &str, status: &str, n: u32) -> Note {
    Note {
        session: session.into(),
        status: status.into(),
        n,
    }
}

#[test]
fn the_native_api_stores_reads_updates_queries_and_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.felt");
    let db = FeltDb::open(&path).expect("a store opens in a fresh temporary directory");

    // Write, then read back.
    db.insert("session:s1", note("s1", "running", 1)).unwrap();
    assert_eq!(
        db.get::<Note>("session:s1").unwrap(),
        Some(note("s1", "running", 1))
    );

    // Update, then read back.
    db.update("session:s1", note("s1", "failed", 2)).unwrap();
    assert_eq!(
        db.get::<Note>("session:s1").unwrap().unwrap().status,
        "failed"
    );

    // Query with the supported bounded collection scan.
    db.insert("session:s2", note("s2", "running", 3)).unwrap();
    db.insert("attempt:a1", json!({"session": "s1", "outcome": "failed"}))
        .unwrap();
    let running = db
        .query_collection("session", None, |row| row.value["status"] == "running")
        .unwrap();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].key, "session:s2");
    assert_eq!(
        db.list_collection_page("session", None, 10).unwrap().len(),
        2
    );

    // Delete: logically absent from reads, queries and cardinality.
    db.delete("session:s2").unwrap();
    assert_eq!(db.get::<Note>("session:s2").unwrap(), None);
    assert!(
        db.query_collection("session", None, |_| true)
            .unwrap()
            .iter()
            .all(|r| r.key != "session:s2")
    );
    assert_eq!(db.collection_cardinality("session").unwrap(), 1);

    // An error path is surfaced as a typed error, not swallowed.
    let duplicate = db.insert_if_absent("session:s1", note("s1", "x", 0));
    assert!(
        matches!(duplicate, Err(FlowError::PreconditionFailed(_))),
        "{duplicate:?}"
    );
    let wrong_type = db.get::<u64>("session:s1");
    assert!(
        matches!(wrong_type, Err(FlowError::Serde(_))),
        "{wrong_type:?}"
    );

    // The filesystem implications of the file journal: a journal and an ownership lock file.
    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["session.felt", "session.felt.lock"]);
}

#[test]
fn dropping_the_store_releases_it_and_a_fresh_store_holds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.felt");
    {
        let db = FeltDb::open(&a).unwrap();
        db.insert("session:leak", note("leak", "running", 1))
            .unwrap();
    } // the only handle is dropped: there is no close(); this is the supported lifecycle

    // The same path reopens (the ownership lock was released) and replays the journal.
    let again = FeltDb::open(&a).expect("ownership is released when the last handle drops");
    assert!(
        again.get::<Note>("session:leak").unwrap().is_some(),
        "the journal is durable across drop"
    );

    // A different path is a different store: nothing leaks into it.
    let b = FeltDb::open(dir.path().join("b.felt")).unwrap();
    assert_eq!(b.get::<Note>("session:leak").unwrap(), None);
    assert_eq!(
        b.list_collection_page("session", None, 10).unwrap().len(),
        0
    );
    assert_eq!(b.sequence().unwrap(), 0);
}
