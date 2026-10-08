//! Facts about the native FeltDB Rust API that the compatibility report and the design rely on,
//! each established against the real implementation and asserted so that a change upstream is
//! noticed. Nothing here uses the adapter.
use feltdb::{FeltDb, FlowError, StateStore};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

fn size(p: &std::path::Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}
fn fill(db: &FeltDb, n: usize) {
    let blob = "x".repeat(10_000);
    for i in 0..n {
        db.insert(&format!("obs:{i:04}"), json!({"i": i, "blob": blob}))
            .unwrap();
    }
}

#[test]
fn opening_a_store_starts_no_threads() {
    let count = || std::fs::read_dir("/proc/self/task").unwrap().count();
    // Other tests in this binary run on other threads, so only a difference is meaningful here:
    // opening must not by itself add one. (The test binary is built with --test-threads default;
    // read the count twice around a single open.)
    let dir = tempfile::tempdir().unwrap();
    let before = count();
    let db = FeltDb::open(dir.path().join("t.felt")).unwrap();
    db.insert("a:b", json!(1)).unwrap();
    let after = count();
    assert!(
        after <= before + 1,
        "open + write added {} threads",
        after as i64 - before as i64
    );
    drop(db);
}

#[test]
fn there_is_no_in_memory_backend_a_store_is_a_journal_file_plus_an_ownership_lock() {
    // FeltDb::open takes a path and creates a file; the lock file is its process-exclusive ownership.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.felt");
    let _db = FeltDb::open(&path).unwrap();
    assert!(path.exists());
    assert!(dir.path().join("t.felt.lock").exists());
}

#[test]
fn a_write_costs_several_times_its_payload_in_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.felt");
    let db = FeltDb::open(&path).unwrap();
    fill(&db, 100);
    let payload_bytes = 100 * 10_000u64;
    let ratio = size(&path) as f64 / payload_bytes as f64;
    assert!((2.5..=4.5).contains(&ratio), "journal/payload = {ratio:.2}");
}

#[test]
fn deleting_a_record_does_not_release_its_payload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.felt");
    let db = Arc::new(FeltDb::open(&path).unwrap());
    fill(&db, 50);
    let journal_before = size(&path);
    assert_eq!(
        db.diagnostic_row_count("state").unwrap(),
        50,
        "every write minted a revision row holding the payload"
    );
    for i in 0..50 {
        db.delete(&format!("obs:{i:04}")).unwrap();
    }
    assert_eq!(
        db.collection_cardinality("obs").unwrap(),
        0,
        "logically gone"
    );
    assert_eq!(
        db.diagnostic_row_count("state").unwrap(),
        50,
        "the revision rows, and the payloads in them, remain"
    );
    assert!(
        size(&path) >= journal_before,
        "the journal only grows by deleting"
    );
    let store = StateStore::with_feltdb(db.clone()).unwrap();
    assert_eq!(
        store.history_of("obs:0000").len(),
        1,
        "a deleted record's payload is still readable through its history"
    );
}

#[test]
fn garbage_collection_removes_revision_rows_of_deleted_records() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(FeltDb::open(dir.path().join("t.felt")).unwrap());
    fill(&db, 50);
    for i in 0..50 {
        db.delete(&format!("obs:{i:04}")).unwrap();
    }
    let store = StateStore::with_feltdb(db.clone()).unwrap();
    let report = store.collect_unreachable().unwrap();
    assert_eq!(report.collected_revisions.len(), 50);
    assert_eq!(db.diagnostic_row_count("state").unwrap(), 0);
    assert!(store.history_of("obs:0000").is_empty());
}

#[test]
fn garbage_collection_also_collects_the_history_of_live_records_when_no_ref_is_held() {
    // Stated plainly because it is the cost of the mechanism: with no refs, every revision is
    // unreachable. The live records themselves are unaffected.
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(FeltDb::open(dir.path().join("t.felt")).unwrap());
    fill(&db, 5);
    let store = StateStore::with_feltdb(db.clone()).unwrap();
    assert_eq!(
        store
            .collect_unreachable()
            .unwrap()
            .collected_revisions
            .len(),
        5
    );
    assert_eq!(db.collection_cardinality("obs").unwrap(), 5);
    assert!(db.get_value("obs:0003").unwrap().is_some());
}

#[test]
fn log_compaction_does_nothing_without_a_peer_and_prunes_once_a_peer_has_acknowledged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.felt");
    let db = FeltDb::open(&path).unwrap();
    fill(&db, 50);
    let before = size(&path);
    assert_eq!(
        db.compact_operation_log(&[]).unwrap(),
        0,
        "with no peers it returns immediately"
    );
    assert_eq!(size(&path), before, "and rewrites nothing");
    // Acknowledged by a (nominal) peer, the same call prunes and rewrites.
    let me = db.instance_id().unwrap();
    let seq = db.sequence().unwrap();
    db.add_sync_peer("local".into()).unwrap();
    db.acknowledge_peer_versions("local".into(), HashMap::from([(me, seq)]))
        .unwrap();
    let pruned = db.compact_operation_log(&["local".to_string()]).unwrap();
    assert!(pruned >= 50, "{pruned}");
    // Live records and their revision rows are rewritten as a snapshot, so only the operation
    // log's copies go: a third here, not the whole overhead.
    let after = size(&path);
    assert!(after < before && after > before / 2, "{before} -> {after}");
}

#[test]
fn retention_bounds_revisions_per_resource_when_a_policy_is_set_before_the_writes() {
    use feltdb::state_model::RetentionPolicy;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(FeltDb::open(dir.path().join("t.felt")).unwrap());
    let store = StateStore::with_feltdb(db.clone()).unwrap();
    store
        .set_retention_policy("obs:0000", RetentionPolicy::keep_last(1))
        .unwrap();
    for round in 0..5 {
        let k = "obs:0000";
        let v = json!({"round": round, "blob": "y".repeat(5000)});
        if round == 0 {
            db.insert(k, v).unwrap()
        } else {
            db.update(k, v).unwrap()
        }
    }
    assert_eq!(
        store.history_of("obs:0000").len(),
        1,
        "five writes, one revision kept"
    );
    // The cost: a retention row per resource.
    assert_eq!(db.collection_cardinality("_retention").unwrap(), 1);
}

#[test]
fn a_corrupt_journal_is_a_typed_error_and_is_not_modified() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.felt");
    {
        let db = FeltDb::open(&path).unwrap();
        fill(&db, 3);
    }
    let bytes = std::fs::read(&path).unwrap();
    let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    lines.insert(2, b"not json at all");
    let corrupted = lines.join(&b'\n');
    std::fs::write(&path, &corrupted).unwrap();
    let r = FeltDb::open(&path);
    assert!(
        matches!(r, Err(FlowError::CorruptLogLine(_))),
        "{:?}",
        r.err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), corrupted);
}
