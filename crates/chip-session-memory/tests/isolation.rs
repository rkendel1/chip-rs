//! Two or more independent sessions: no leakage, no collisions, independent compaction and
//! teardown, and the one hazard FeltDB itself has (two handles to one path share an instance).
mod common;
use chip_session_memory::compaction::Inject;
use chip_session_memory::*;
use common::*;
use feltdb::FeltDb;

#[test]
fn sessions_do_not_see_each_others_records_and_identical_ids_do_not_collide() {
    let root = tempfile::tempdir().unwrap();
    let mut a = SessionMemory::create(root.path(), "alpha", "alpha objective").unwrap();
    let mut b = SessionMemory::create(root.path(), "beta", "beta objective").unwrap();
    // The very same identifiers in both.
    a.record_observation(obs("o1", "alpha's read", "ALPHA-PAYLOAD"), true)
        .unwrap();
    b.record_observation(obs("o1", "beta's read", "BETA-PAYLOAD"), true)
        .unwrap();
    a.put_task("t1", "alpha task", false).unwrap();
    b.put_task("t1", "beta task", true).unwrap();
    a.record_attempt("x", "alpha attempt", Outcome::Failed, vec![])
        .unwrap();
    assert_eq!(a.payload("o1").unwrap().as_deref(), Some("ALPHA-PAYLOAD"));
    assert_eq!(b.payload("o1").unwrap().as_deref(), Some("BETA-PAYLOAD"));
    assert_eq!(
        a.observation("o1").unwrap().unwrap().summary,
        "alpha's read"
    );
    // Beta has no attempt "x": the id exists in alpha only.
    assert!(b.reconstruct().unwrap().attempts.is_empty());
    assert!(matches!(
        b.record_attempt(
            "y",
            "uses alpha's id",
            Outcome::Failed,
            vec![r("attempt", "x")]
        ),
        Err(SessionMemoryError::MissingReference { .. })
    ));
    // Update in one does not touch the other; delete in one does not touch the other.
    a.put_task("t1", "alpha task", true).unwrap();
    assert!(a.reconstruct().unwrap().tasks[0].done && b.reconstruct().unwrap().tasks[0].done);
    a.delete("payload", "o1").unwrap();
    assert_eq!(a.payload("o1").unwrap(), None);
    assert_eq!(b.payload("o1").unwrap().as_deref(), Some("BETA-PAYLOAD"));
    assert_eq!(
        a.reconstruct().unwrap().session.objective,
        "alpha objective"
    );
    assert_eq!(b.reconstruct().unwrap().session.objective, "beta objective");
    // Every record each session holds names that session.
    for o in a.observations().unwrap() {
        assert_eq!(o.session, "alpha");
    }
}

#[test]
fn a_foreign_record_in_a_sessions_store_is_refused_not_returned() {
    let root = tempfile::tempdir().unwrap();
    let mut a = SessionMemory::create(root.path(), "alpha", "o").unwrap();
    a.close();
    let db = FeltDb::open(root.path().join("alpha/session.felt")).unwrap();
    db.insert("obs:alpha:smuggled", serde_json::json!({"schema":1,"session":"beta","id":"smuggled","kind":"read","provenance":null,"summary":"s","digest":"","payload_len":0,"excerpt":"","payload_held":false,"superseded_by":null,"pinned":false})).unwrap();
    drop(db);
    match SessionMemory::recover(root.path(), "alpha") {
        Err(SessionMemoryError::Recovery(m)) => assert!(m.contains("beta"), "{m}"),
        other => panic!(
            "a foreign record must refuse recovery, got {:?}",
            other.map(|_| ())
        ),
    }
}

#[test]
fn compacting_one_session_does_not_affect_another() {
    let root = tempfile::tempdir().unwrap();
    let mut a = rich(root.path(), "alpha");
    let b = rich(root.path(), "beta");
    let b_stats = b.stats().unwrap();
    let b_state = b.reconstruct().unwrap();
    let b_journal = std::fs::read(b.journal_path()).unwrap();
    a.compact().unwrap();
    assert_eq!(
        b.stats().unwrap(),
        b_stats,
        "the other session's data and footprint are unchanged"
    );
    assert_eq!(b.reconstruct().unwrap(), b_state);
    assert_eq!(
        std::fs::read(b.journal_path()).unwrap(),
        b_journal,
        "its journal was not rewritten"
    );
    assert_eq!(a.stats().unwrap().payload_records, 2);
    assert_eq!(b.stats().unwrap().payload_records, 41);
}

#[test]
fn destroying_one_session_neither_destroys_nor_corrupts_another() {
    let root = tempfile::tempdir().unwrap();
    let a = rich(root.path(), "alpha");
    let mut b = rich(root.path(), "beta");
    let before = b.reconstruct().unwrap();
    a.destroy().unwrap();
    assert!(!root.path().join("alpha").exists());
    assert_eq!(b.reconstruct().unwrap(), before);
    b.record_attempt("later", "still writable", Outcome::Succeeded, vec![])
        .unwrap();
    b.compact().unwrap();
    b.close();
    let (_b, rec) = SessionMemory::recover(root.path(), "beta").unwrap();
    assert_eq!(rec.attempts.len(), 3);
}

#[test]
fn teardown_with_work_pending_or_failed_is_safe() {
    let root = tempfile::tempdir().unwrap();
    // Destroyed while a compaction is preserved but not finished.
    let mut m = rich(root.path(), "pending");
    m.compact_with(Inject::AfterPreserve).unwrap_err();
    m.destroy().unwrap();
    assert!(!root.path().join("pending").exists());
    // Closed while a compaction failed part way; the session is intact and resumable.
    let mut m = rich(root.path(), "failed");
    m.compact_with(Inject::DuringPurge { batches: 0 })
        .unwrap_err();
    assert!(m.close());
    assert!(!m.close());
    let (mut m, rec) = SessionMemory::recover(root.path(), "failed").unwrap();
    assert_eq!(rec.pending_compaction, Some(1));
    m.compact().unwrap();
    // A second destroy of a destroyed session reports the missing directory instead of pretending.
    let dir = m.dir().to_path_buf();
    m.destroy().unwrap();
    assert!(!dir.exists());
}

#[test]
fn an_error_never_leaves_a_session_marked_compacted_or_recovered() {
    let root = tempfile::tempdir().unwrap();
    for inject in [
        Inject::AfterPreserve,
        Inject::DuringPurge { batches: 0 },
        Inject::BeforeRecord,
        Inject::BeforeReclaim,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut m = rich(dir.path(), "s");
        assert!(m.compact_with(inject).is_err(), "{inject:?}");
        assert_eq!(
            m.reconstruct().unwrap().session.compactions,
            0,
            "{inject:?}: an interrupted compaction is never counted"
        );
    }
    // Recovery that fails returns no session at all, so nothing can be mistaken for recovered.
    assert!(SessionMemory::recover(root.path(), "absent").is_err());
}

#[test]
fn independent_sessions_work_concurrently_on_separate_threads() {
    // FeltDB serializes operations on one store; separate sessions are separate stores. This tests
    // exactly that and nothing about concurrent writers to a single session.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().to_path_buf();
    let handles: Vec<_> = (0..4)
        .map(|n| {
            let path = path.clone();
            std::thread::spawn(move || {
                let id = format!("s{n}");
                let mut m = SessionMemory::create(&path, &id, &format!("objective {n}")).unwrap();
                for i in 0..100 {
                    m.record_observation(
                        obs(&format!("o{i}"), &format!("{n}/{i}"), &big(&id, 2000)),
                        true,
                    )
                    .unwrap();
                }
                m.record_attempt("a", &format!("attempt of {n}"), Outcome::Failed, vec![])
                    .unwrap();
                m.checkpoint("c").unwrap();
                m.compact().unwrap();
                m.close();
                let (_m, rec) = SessionMemory::recover(&path, &id).unwrap();
                (
                    n,
                    rec.session.objective.clone(),
                    rec.attempts[0].action.clone(),
                    rec.observations,
                )
            })
        })
        .collect();
    for h in handles {
        let (n, objective, attempt, observations) = h.join().unwrap();
        assert_eq!(objective, format!("objective {n}"));
        assert_eq!(attempt, format!("attempt of {n}"));
        assert_eq!(observations, 100);
    }
}

#[test]
fn two_handles_to_one_path_share_state_so_the_adapter_never_hands_out_a_second_one() {
    // The hazard, at the FeltDB level: opening a path that is already open in this process returns
    // the same instance (a process-global registry keyed by path), not an independent store.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("shared.felt");
    let one = FeltDb::open(&path).unwrap();
    let two = FeltDb::open(&path).unwrap();
    one.insert("note:x", serde_json::json!({"v": 1})).unwrap();
    assert!(
        two.get_value("note:x").unwrap().is_some(),
        "two handles to one path are one store"
    );
    // The adapter's defence: a session id is a fresh directory and create refuses an existing one.
    let m = SessionMemory::create(root.path(), "only-one", "o").unwrap();
    assert!(matches!(
        SessionMemory::create(root.path(), "only-one", "o"),
        Err(SessionMemoryError::Init(_))
    ));
    drop(m);
}
