//! Schema, validation, lifecycle and the explicit statement of what the native API does not offer.
mod common;
use chip_session_memory::*;
use common::*;
use feltdb::FeltDb;

#[test]
fn records_are_created_read_updated_queried_and_deleted_per_session() {
    let root = tempfile::tempdir().unwrap();
    let mut m = SessionMemory::create(root.path(), "s1", "objective").unwrap();
    m.put_task("t1", "first goal", false).unwrap();
    m.put_task("t1", "first goal", true).unwrap(); // update
    m.put_task("t2", "second goal", false).unwrap();
    let rec = m.reconstruct().unwrap();
    assert_eq!(rec.tasks.len(), 2);
    assert!(rec.tasks.iter().find(|t| t.id == "t1").unwrap().done);
    assert_eq!(m.set_plan(vec!["a".into()]).unwrap(), 1);
    assert_eq!(m.set_plan(vec!["a".into(), "b".into()]).unwrap(), 2);
    assert_eq!(m.reconstruct().unwrap().plan.unwrap().revision, 2);
    // Query by predicate.
    m.record_observation(obs("o1", "s", "payload"), true)
        .unwrap();
    assert_eq!(m.observations().unwrap().len(), 1);
    // Delete: working-class and transient only.
    m.delete("task", "t2").unwrap();
    assert_eq!(m.reconstruct().unwrap().tasks.len(), 1);
    m.delete("payload", "o1").unwrap();
    assert_eq!(m.payload("o1").unwrap(), None);
    assert!(
        !m.observation("o1").unwrap().unwrap().payload_held,
        "the record never claims a missing payload"
    );
    let refused = m.delete("attempt", "a1");
    assert!(
        matches!(refused, Err(SessionMemoryError::Invalid(_))),
        "recovery-class records are not deletable: {refused:?}"
    );
}

#[test]
fn identifiers_and_records_are_validated() {
    let root = tempfile::tempdir().unwrap();
    for bad in ["", "a:b", "has space", &"x".repeat(65), "ünï"] {
        assert!(
            matches!(
                SessionMemory::create(root.path(), bad, "o"),
                Err(SessionMemoryError::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(
        SessionMemory::create(root.path(), "ok", "   "),
        Err(SessionMemoryError::Invalid(_))
    ));
    let mut m = SessionMemory::create(root.path(), "s", "o").unwrap();
    assert!(matches!(
        m.put_task("a:b", "g", false),
        Err(SessionMemoryError::Invalid(_))
    ));
    assert!(matches!(
        m.record_hypothesis("h", "e", vec![], Some(1001)),
        Err(SessionMemoryError::Invalid(_))
    ));
    assert!(matches!(
        m.record_attempt("a", "x", Outcome::Failed, vec![r("nonsense", "o1")]),
        Err(SessionMemoryError::Invalid(_))
    ));
}

#[test]
fn a_reference_to_something_missing_is_refused_not_invented() {
    let root = tempfile::tempdir().unwrap();
    let mut m = SessionMemory::create(root.path(), "s", "o").unwrap();
    assert!(matches!(
        m.record_attempt("a", "x", Outcome::Failed, vec![r("obs", "ghost")]),
        Err(SessionMemoryError::MissingReference { .. })
    ));
    assert!(matches!(
        m.record_test_result("t", "unit", false, vec![], Some("ghost")),
        Err(SessionMemoryError::MissingReference { .. })
    ));
    assert!(matches!(
        m.record_escalation("e", "why", vec!["ghost".into()], vec![], vec![], vec![]),
        Err(SessionMemoryError::MissingReference { .. })
    ));
    assert!(matches!(
        m.supersede_observation("ghost", "ghost2"),
        Err(SessionMemoryError::MissingReference { .. })
    ));
    // Nothing was half-written.
    assert!(m.reconstruct().unwrap().attempts.is_empty());
}

#[test]
fn a_repair_is_verified_only_by_a_passing_recorded_test_result() {
    let root = tempfile::tempdir().unwrap();
    let mut m = SessionMemory::create(root.path(), "s", "o").unwrap();
    m.record_observation(obs("o1", "s", "p"), true).unwrap();
    m.record_test_result("fail", "unit", false, vec!["x".into()], Some("o1"))
        .unwrap();
    m.record_test_result("pass", "other", true, vec![], Some("o1"))
        .unwrap();
    assert!(matches!(
        m.record_repair("r1", "fix", Outcome::Succeeded, Some("missing")),
        Err(SessionMemoryError::MissingReference { .. })
    ));
    assert!(
        matches!(
            m.record_repair("r1", "fix", Outcome::Succeeded, Some("fail")),
            Err(SessionMemoryError::Invalid(_))
        ),
        "a failing result cannot verify"
    );
    m.record_repair("r1", "fix", Outcome::Succeeded, Some("pass"))
        .unwrap();
    m.record_repair("r2", "fix, unverified", Outcome::Succeeded, None)
        .unwrap();
    let rec = m.reconstruct().unwrap();
    assert_eq!(
        rec.verified_repairs.len(),
        1,
        "only the repair backed by a passing result counts as verified"
    );
    assert_eq!(rec.repairs.len(), 2);
}

#[test]
fn a_provenance_string_is_kept_verbatim_and_never_minted_or_derived() {
    let root = tempfile::tempdir().unwrap();
    let mut m = SessionMemory::create(root.path(), "the-session", "o").unwrap();
    m.record_observation(
        NewObservation {
            provenance: Some("whatever-another-authority-said".into()),
            ..obs("o1", "s", "p")
        },
        true,
    )
    .unwrap();
    m.record_observation(
        NewObservation {
            provenance: None,
            ..obs("o2", "s", "p")
        },
        true,
    )
    .unwrap();
    assert_eq!(
        m.observation("o1").unwrap().unwrap().provenance.as_deref(),
        Some("whatever-another-authority-said")
    );
    assert_eq!(
        m.observation("o2").unwrap().unwrap().provenance,
        None,
        "no provenance is not replaced by an invented one"
    );
}

#[test]
fn close_is_idempotent_and_a_closed_session_refuses_work() {
    let root = tempfile::tempdir().unwrap();
    let mut m = SessionMemory::create(root.path(), "s", "o").unwrap();
    m.put_task("t", "g", false).unwrap();
    assert!(!m.is_closed());
    assert!(m.close(), "the first close does the closing");
    assert!(!m.close(), "closing again is safe and does nothing");
    assert!(m.is_closed());
    assert!(matches!(
        m.put_task("t2", "g", false),
        Err(SessionMemoryError::Closed)
    ));
    assert!(matches!(
        m.reconstruct(),
        Err(SessionMemoryError::Recovery(_)) | Err(SessionMemoryError::Closed)
    ));
    assert!(matches!(m.stats(), Err(SessionMemoryError::Closed)));
    assert!(matches!(
        m.compact(),
        Err(SessionMemoryError::Closed) | Err(SessionMemoryError::Compaction { .. })
    ));
    // The ownership lock was released: the session can be recovered.
    let (again, rec) = SessionMemory::recover(root.path(), "s").unwrap();
    assert_eq!(rec.tasks.len(), 1);
    drop(again);
}

#[test]
fn destroy_removes_the_files_and_the_session_cannot_then_be_recovered() {
    let root = tempfile::tempdir().unwrap();
    let m = SessionMemory::create(root.path(), "gone", "o").unwrap();
    let dir = m.dir().to_path_buf();
    assert!(dir.exists());
    m.destroy().unwrap();
    assert!(!dir.exists());
    assert!(matches!(
        SessionMemory::recover(root.path(), "gone"),
        Err(SessionMemoryError::Recovery(_))
    ));
    // The id is free again.
    SessionMemory::create(root.path(), "gone", "o").unwrap();
}

#[test]
fn initialization_failures_are_typed_and_leave_nothing_behind() {
    let root = tempfile::tempdir().unwrap();
    // The root is a file: the directory cannot be created.
    let file = root.path().join("not-a-dir");
    std::fs::write(&file, "x").unwrap();
    assert!(matches!(
        SessionMemory::create(&file, "s", "o"),
        Err(SessionMemoryError::Init(_))
    ));
    assert!(file.is_file(), "an unrelated file is untouched");

    // A failure after the store was opened and written removes what this call created.
    let dir = root.path().join("partial");
    let failed = SessionMemory::create_failing_after_open(root.path(), "partial", "o");
    assert!(matches!(failed, Err(SessionMemoryError::Init(_))));
    assert!(
        !dir.exists(),
        "the partially created session directory is removed"
    );
    SessionMemory::create(root.path(), "partial", "o")
        .expect("the id is usable afterwards, so the lock was released");

    // An existing session is never overwritten or removed by a failed create.
    let mut keep = SessionMemory::create(root.path(), "keep", "o").unwrap();
    keep.put_task("t", "g", false).unwrap();
    keep.close();
    assert!(matches!(
        SessionMemory::create(root.path(), "keep", "o"),
        Err(SessionMemoryError::Init(_))
    ));
    let (_m, rec) = SessionMemory::recover(root.path(), "keep").unwrap();
    assert_eq!(
        rec.tasks.len(),
        1,
        "the existing session survived the refused create"
    );
}

#[test]
fn unsupported_native_capabilities_are_stated_not_emulated() {
    let c = capabilities();
    assert!(
        matches!(c.in_memory_backend, Support::Unsupported(why) if why.contains("no in-memory backend"))
    );
    assert!(matches!(c.explicit_close, Support::Unsupported(_)));
    assert!(matches!(
        c.physical_reclaim_without_replication,
        Support::Unsupported(_)
    ));
    assert!(matches!(
        c.concurrent_writers_to_one_session,
        Support::Unsupported(_)
    ));
    assert_eq!(c.durable_file_journal, Support::Supported);
    assert_eq!(c.atomic_batches, Support::Supported);
}

#[test]
fn a_session_written_by_a_newer_schema_is_refused_not_misread() {
    let root = tempfile::tempdir().unwrap();
    let mut m = SessionMemory::create(root.path(), "s", "o").unwrap();
    m.put_task("t", "g", false).unwrap();
    m.close();
    let db = FeltDb::open(root.path().join("s/session.felt")).unwrap();
    let mut meta: serde_json::Value = db.get_value("session:s:meta").unwrap().unwrap();
    meta["schema"] = serde_json::json!(99);
    db.update("session:s:meta", meta).unwrap();
    drop(db);
    let r = SessionMemory::recover(root.path(), "s");
    assert!(
        matches!(&r, Err(SessionMemoryError::Recovery(m)) if m.contains("newer")),
        "{:?}",
        r.err()
    );
}
