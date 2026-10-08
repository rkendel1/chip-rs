//! Checkpoint and recovery from persisted data: after the instance is destroyed, after a
//! compaction, after a crash, and the ways recovery must refuse.
mod common;
use chip_session_memory::*;
use common::*;
use feltdb::FeltDb;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn journal(root: &std::path::Path, s: &str) -> std::path::PathBuf {
    root.join(s).join("session.felt")
}

#[test]
fn a_compacted_session_is_reconstructed_from_disk_after_the_instance_is_destroyed() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    let cp = m.checkpoint("c1").unwrap();
    assert!(!cp.state_digest.is_empty() && cp.sequence > 0);
    let original = m.reconstruct().unwrap();
    let journal_before = std::fs::metadata(journal(root.path(), "s")).unwrap().len();
    let report = m.compact().unwrap();
    assert_eq!(report.payloads_purged, 39);
    let sequence_at_compaction = m.reconstruct().unwrap();
    // Destroy the instance: no handle, no in-memory object of the session survives.
    drop(sequence_at_compaction);
    m.close();
    drop(m);
    drop(original.clone());
    assert!(
        journal(root.path(), "s").exists(),
        "the persisted journal is all that is left"
    );
    let bytes_on_disk = std::fs::metadata(journal(root.path(), "s")).unwrap().len();
    assert!(
        bytes_on_disk < journal_before / 4,
        "the rewritten journal holds the compacted session, not its payloads: {journal_before} -> {bytes_on_disk}"
    );

    let (m, rec) = SessionMemory::recover(root.path(), "s").unwrap();
    // Objective, task status, plan.
    assert_eq!(
        rec.session.objective,
        "make the integration tests pass without weakening any test"
    );
    assert_eq!(rec.session.status, SessionStatus::Escalated);
    assert_eq!(rec.tasks[0].goal, "fix route inheritance");
    assert!(!rec.tasks[0].done);
    assert_eq!(
        rec.plan.as_ref().unwrap().steps,
        vec!["read", "fix", "retest"]
    );
    // Attempts, failures and successful repairs, with their verification.
    assert_eq!(rec.attempts.len(), 2);
    assert_eq!(
        rec.failed_approaches
            .iter()
            .map(|a| a.action.as_str())
            .collect::<Vec<_>>(),
        vec!["edit executor"]
    );
    assert_eq!(rec.verified_repairs.len(), 1);
    assert_eq!(rec.verified_repairs[0].verified_by.as_deref(), Some("tr3"));
    // Test failures and their diagnostic references; unresolved questions and hypotheses.
    assert_eq!(rec.unresolved_failures.len(), 1);
    assert_eq!(
        rec.unresolved_failures[0].failed_tests,
        vec!["integration::lenient"]
    );
    assert_eq!(
        rec.unresolved_failures[0].diagnostic.as_deref(),
        Some("o030")
    );
    assert_eq!(
        rec.open_hypotheses
            .iter()
            .map(|h| h.id.as_str())
            .collect::<Vec<_>>(),
        vec!["h1"]
    );
    // Escalation context and the pending decision.
    assert_eq!(
        rec.escalations[0].reason,
        "two conventions disagree; tests cannot arbitrate"
    );
    assert_eq!(rec.escalations[0].prior_attempts, vec!["a1", "a2"]);
    assert_eq!(
        rec.escalations[0].known_failures,
        vec!["strict retry keys fail config_lenient"]
    );
    assert_eq!(rec.escalations[0].outstanding, vec!["warn or fail?"]);
    assert!(rec.session.pending.as_ref().unwrap().decision_needed);
    // Checkpoint and its evidence references all resolve (reconstruct would have refused otherwise).
    assert_eq!(rec.checkpoint.as_ref().unwrap().id, "c1");
    assert!(
        rec.checkpoint
            .as_ref()
            .unwrap()
            .refs
            .contains(&r("attempt", "a1"))
    );
    assert_eq!(rec.session.compactions, 1);
    // Purged payloads are absent; what survives is internally consistent.
    assert_eq!(m.payload("o005").unwrap(), None);
    let kept = m.observation("o030").unwrap().unwrap();
    let text = m.payload("o030").unwrap().unwrap();
    assert!(kept.payload_held && kept.pinned);
    assert_eq!(kept.payload_len as usize, text.len());
    assert_eq!(kept.digest.len(), 64);
    for o in m.observations().unwrap() {
        assert_eq!(
            o.payload_held,
            m.payload(&o.id).unwrap().is_some(),
            "{} agrees with storage",
            o.id
        );
    }
}

#[test]
fn a_session_can_be_worked_on_after_recovery() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    m.compact().unwrap();
    m.close();
    let (mut m, _) = SessionMemory::recover(root.path(), "s").unwrap();
    m.record_attempt(
        "a3",
        "apply the human decision",
        Outcome::Succeeded,
        vec![r("attempt", "a2")],
    )
    .unwrap();
    m.record_test_result("tr5", "integration", true, vec![], Some("o030"))
        .unwrap();
    m.record_repair(
        "rp2",
        "warn on unknown retry keys",
        Outcome::Succeeded,
        Some("tr5"),
    )
    .unwrap();
    m.set_pending(None).unwrap();
    m.set_status(SessionStatus::Completed).unwrap();
    m.checkpoint("c2").unwrap();
    m.close();
    let (_m, rec) = SessionMemory::recover(root.path(), "s").unwrap();
    assert_eq!(rec.session.status, SessionStatus::Completed);
    assert!(
        rec.unresolved_failures.is_empty(),
        "the failure was superseded by a passing reading"
    );
    assert_eq!(rec.verified_repairs.len(), 2);
    assert_eq!(rec.checkpoint.unwrap().id, "c2");
}

#[test]
fn recovery_refuses_what_it_cannot_reconstruct() {
    let root = tempfile::tempdir().unwrap();
    // A session that was never persisted.
    assert!(matches!(
        SessionMemory::recover(root.path(), "never"),
        Err(SessionMemoryError::Recovery(_))
    ));
    // An invalid id is refused before the filesystem is consulted.
    assert!(matches!(
        SessionMemory::recover(root.path(), "../etc"),
        Err(SessionMemoryError::Invalid(_))
    ));
}

#[test]
fn a_corrupted_journal_is_refused_and_left_untouched() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    m.close();
    let path = journal(root.path(), "s");
    let original = std::fs::read(&path).unwrap();
    // Corruption in the middle of the log, which FeltDB refuses to open.
    let mut lines: Vec<&[u8]> = original.split(|b| *b == b'\n').collect();
    let mid = lines.len() / 2;
    lines.insert(mid, b"{ this is not a record");
    let corrupted = lines.join(&b'\n');
    std::fs::write(&path, &corrupted).unwrap();
    let r = SessionMemory::recover(root.path(), "s");
    assert!(
        matches!(r, Err(SessionMemoryError::Recovery(_))),
        "{:?}",
        r.err()
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        corrupted,
        "a refused open modifies nothing"
    );
}

#[test]
fn an_unterminated_final_write_is_discarded_and_the_session_recovers() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    m.checkpoint("c1").unwrap();
    let before = m.reconstruct().unwrap();
    m.close();
    // A crash in the middle of an append leaves a partial final record.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(journal(root.path(), "s"))
        .unwrap();
    f.write_all(b"{\"capability\":\"obs\",\"key\":\"obs:s:half")
        .unwrap();
    drop(f);
    let (_m, rec) = SessionMemory::recover(root.path(), "s").unwrap();
    assert_eq!(
        rec, before,
        "everything acknowledged survived; the half-written record did not"
    );
}

#[test]
fn inconsistent_persisted_data_is_reported_as_such() {
    // Each case tampers with one thing behind the adapter's back and expects a named refusal.
    type Tamper = fn(&FeltDb);
    let cases: Vec<(&str, Tamper, &str)> = vec![
        (
            "a checkpoint reference is missing",
            |db| db.delete("task:s:t1").unwrap(),
            "checkpoint c1 references missing task t1",
        ),
        (
            "an escalation cites a missing attempt",
            |db| db.delete("attempt:s:a1").unwrap(),
            "missing attempt a1",
        ),
        (
            "a repair is verified by a missing result",
            |db| {
                let mut v = db.get_value("repair:s:rp1").unwrap().unwrap();
                v["verified_by"] = serde_json::json!("ghost");
                db.update("repair:s:rp1", v).unwrap()
            },
            "missing test result ghost",
        ),
        (
            "a repair is verified by a failing result",
            |db| {
                let mut v = db.get_value("repair:s:rp1").unwrap().unwrap();
                v["verified_by"] = serde_json::json!("tr1");
                db.update("repair:s:rp1", v).unwrap()
            },
            "did not pass",
        ),
        (
            "a test result cites a missing observation",
            |db| db.delete("obs:s:o012").unwrap(),
            "test result tr3 cites missing observation o012",
        ),
        (
            "a record belongs to another session",
            |db| {
                db.insert("attempt:s:intruder", serde_json::json!({"schema": 1, "session": "other", "id": "intruder", "action": "x", "outcome": "failed", "refs": []})).unwrap()
            },
            "other",
        ),
        (
            "the checkpoint record is gone",
            |db| db.delete("ckpt:s:c1").unwrap(),
            "checkpoint c1 is missing",
        ),
        (
            "a pinned payload is gone",
            |db| db.delete("payload:s:o030").unwrap(),
            "lost its payload",
        ),
    ];
    for (name, tamper, expect) in cases {
        let root = tempfile::tempdir().unwrap();
        let mut m = rich(root.path(), "s");
        m.checkpoint("c1").unwrap();
        m.compact().unwrap();
        m.checkpoint("c1").unwrap();
        m.close();
        let db = FeltDb::open(journal(root.path(), "s")).unwrap();
        tamper(&db);
        drop(db);
        let r = SessionMemory::recover(root.path(), "s");
        match r {
            Err(SessionMemoryError::Recovery(msg)) => {
                assert!(msg.contains(expect), "{name}: {msg}")
            }
            other => panic!(
                "{name}: expected a recovery refusal, got {:?}",
                other.map(|_| "a session")
            ),
        }
    }
}

// --- a real process, really killed ---------------------------------------------------------

const CHILD: &str = "SESSION_MEMORY_CHILD_ROOT";

/// The child half of the kill test. In a normal run this does nothing.
#[test]
fn child_writes_until_killed() {
    let Some(root) = std::env::var_os(CHILD) else {
        return;
    };
    let mut m =
        SessionMemory::create(std::path::Path::new(&root), "k", "survive being killed").unwrap();
    for i in 0..50 {
        m.record_observation(obs(&format!("o{i:03}"), "read", &big("p", 4000)), true)
            .unwrap();
    }
    m.record_attempt("a1", "first try", Outcome::Failed, vec![r("obs", "o001")])
        .unwrap();
    m.checkpoint("c1").unwrap();
    println!("READY");
    std::io::stdout().flush().unwrap();
    let mut i = 50;
    loop {
        m.record_observation(obs(&format!("o{i:03}"), "read", &big("p", 4000)), true)
            .unwrap();
        i += 1;
    }
}

#[test]
fn a_killed_process_leaves_a_session_that_recovers_to_what_it_had_acknowledged() {
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_writes_until_killed",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, root.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // libtest prints `test <name> ... ` without a newline under --nocapture, so READY shares a
    // line with it. Read on a thread with a deadline so a stuck child fails the test, not hangs it.
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
            if line.contains("READY") {
                let _ = tx.send(());
                break;
            }
        }
    });
    if rx.recv_timeout(std::time::Duration::from_secs(60)).is_err() {
        let _ = child.kill();
        panic!("the child never reported READY");
    }
    // It is now writing continuously. Kill it without any chance to clean up.
    std::thread::sleep(std::time::Duration::from_millis(150));
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "the child did not exit normally: it was killed"
    );

    // The ownership lock died with the process, so the session can be reopened.
    let (mut m, rec) = SessionMemory::recover(root.path(), "k").expect("recovery after SIGKILL");
    assert_eq!(rec.session.objective, "survive being killed");
    assert_eq!(rec.checkpoint.as_ref().unwrap().id, "c1");
    assert_eq!(rec.failed_approaches[0].action, "first try");
    assert!(
        rec.observations >= 50,
        "everything acknowledged before the checkpoint survived: {}",
        rec.observations
    );
    // Whatever the kill interrupted is consistent: a record never claims a payload that is missing.
    for o in m.observations().unwrap() {
        if o.payload_held {
            assert!(
                m.payload(&o.id).unwrap().is_some(),
                "{} claims a payload that is not stored",
                o.id
            );
        }
    }
    // And the recovered session can be compacted (any orphan payload from the interrupted write is
    // purged) and checkpointed again.
    m.compact().unwrap();
    m.checkpoint("c2").unwrap();
    assert_eq!(m.stats().unwrap().payload_records, 0);
}
