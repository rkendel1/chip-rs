//! Retention, compaction and purge: preservation before deletion, idempotence, interruption,
//! tamper detection, and the difference between logical deletion and physical reclamation.
mod common;
use chip_session_memory::compaction::Inject;
use chip_session_memory::*;
use common::*;
use feltdb::FeltDb;

/// What recovery must still have, compared across a compaction. Payload bookkeeping is excluded:
/// that is exactly what compaction changes.
fn recovery_view(rec: &RecoveredSession) -> String {
    let mut v = serde_json::json!({
        "objective": rec.session.objective, "status": rec.session.status, "pending": rec.session.pending,
        "tasks": rec.tasks, "plan": rec.plan, "attempts": rec.attempts, "failed": rec.failed_approaches,
        "repairs": rec.repairs, "verified": rec.verified_repairs, "unresolved": rec.unresolved_failures,
        "hyp": rec.hypotheses, "open": rec.open_hypotheses, "esc": rec.escalations,
        "ckpt": rec.checkpoint.as_ref().map(|c| (&c.id, &c.refs)), "observations": rec.observations,
    });
    v.as_object_mut().unwrap();
    v.to_string()
}

#[test]
fn compaction_purges_transient_payloads_and_preserves_everything_recovery_needs() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    m.checkpoint("c1").unwrap();
    let before = m.reconstruct().unwrap();
    let stats_before = m.stats().unwrap();
    assert_eq!(stats_before.payload_records, 41);

    let report = m.compact().unwrap();
    let after = m.reconstruct().unwrap();
    let stats_after = m.stats().unwrap();

    // Recovery information is identical.
    assert_eq!(recovery_view(&before), recovery_view(&after));
    // Two payloads are still needed and pinned: the unresolved failure's diagnostic (o030) and the
    // evidence of the open hypothesis (o020). Everything else is transient.
    assert_eq!(stats_after.payload_records, 2);
    assert_eq!(m.payload("o030").unwrap().unwrap().len(), 6000);
    assert!(m.observation("o030").unwrap().unwrap().pinned);
    assert!(m.observation("o020").unwrap().unwrap().pinned);
    assert_eq!(report.payloads_purged, 39);
    assert_eq!(report.payloads_pinned, 2);
    assert_eq!(report.payload_bytes_purged, 39 * 6000);
    // A purged observation keeps what recovery uses: summary, digest, length, excerpt, provenance.
    let o = m.observation("o005").unwrap().unwrap();
    assert!(!o.payload_held);
    assert_eq!(
        m.payload("o005").unwrap(),
        None,
        "the purged payload is absent"
    );
    assert_eq!(o.summary, "read #5");
    assert_eq!(o.payload_len, 6000);
    assert_eq!(o.digest.len(), 64);
    assert_eq!(o.excerpt.len(), 256);
    assert_eq!(
        o.provenance.as_deref(),
        Some("exec-supplied-by-caller-o005")
    );
    // Reconstructed state still names the failure, the failed approach, the repair and the question.
    assert_eq!(after.unresolved_failures[0].command, "integration");
    assert_eq!(after.failed_approaches[0].id, "a1");
    assert_eq!(
        after.verified_repairs[0].verified_by.as_deref(),
        Some("tr3")
    );
    assert_eq!(after.escalations[0].outstanding, vec!["warn or fail?"]);
    assert_eq!(after.open_hypotheses.len(), 1);
    assert_eq!(
        after.session.pending.as_ref().unwrap().decision_needed,
        true
    );
    // The session is marked compacted only now.
    assert_eq!(after.session.compactions, 1);
}

#[test]
fn compaction_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    let first = m.compact().unwrap();
    let state = recovery_view(&m.reconstruct().unwrap());
    let again = m.compact().unwrap();
    assert!(again.nothing_to_purge);
    assert_eq!(again.payloads_purged, 0);
    assert_eq!(m.reconstruct().unwrap().session.compactions, first.number);
    assert_eq!(recovery_view(&m.reconstruct().unwrap()), state);
    // New work after a compaction is compacted by the next one, and is numbered next.
    m.record_observation(obs("late", "late read", &big("late", 9000)), true)
        .unwrap();
    let third = m.compact().unwrap();
    assert_eq!(third.number, 2);
    assert_eq!(third.payloads_purged, 1);
    assert_eq!(m.payload("late").unwrap(), None);
}

#[test]
fn interrupted_between_preservation_and_purge_nothing_is_lost_and_a_rerun_finishes() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    let err = m.compact_with(Inject::AfterPreserve).unwrap_err();
    assert!(
        matches!(err, SessionMemoryError::Compaction { .. }),
        "{err}"
    );
    // Every payload is still there; the compaction is recorded as preserved, not as done.
    assert_eq!(m.stats().unwrap().payload_records, 41);
    let rec = m.reconstruct().unwrap();
    assert_eq!(rec.pending_compaction, Some(1));
    assert_eq!(
        rec.session.compactions, 0,
        "an interrupted compaction is not marked as done"
    );
    // The interruption survives destroying the instance: a recovered session knows it is pending.
    m.close();
    let (mut m, rec) = SessionMemory::recover(root.path(), "s").unwrap();
    assert_eq!(rec.pending_compaction, Some(1));
    let done = m.compact().unwrap();
    assert!(done.resumed);
    assert_eq!(done.payloads_purged, 39);
    assert_eq!(m.reconstruct().unwrap().pending_compaction, None);
    assert_eq!(m.reconstruct().unwrap().session.compactions, 1);
}

#[test]
fn interrupted_during_the_purge_is_reported_and_a_rerun_completes_exactly() {
    let root = tempfile::tempdir().unwrap();
    let mut m = SessionMemory::create(root.path(), "s", "o").unwrap();
    for i in 0..600 {
        m.record_observation(obs(&format!("o{i:04}"), "read", &big("x", 500)), true)
            .unwrap();
    }
    let err = m
        .compact_with(Inject::DuringPurge { batches: 1 })
        .unwrap_err();
    assert!(
        matches!(
            err,
            SessionMemoryError::Compaction {
                phase: chip_session_memory::CompactionPhase::Purge,
                ..
            }
        ),
        "{err}"
    );
    let mid = m.stats().unwrap();
    assert_eq!(
        mid.payload_records,
        600 - 256,
        "exactly one atomic batch was applied"
    );
    assert_eq!(m.reconstruct().unwrap().session.compactions, 0);
    // Observation records agree with the payloads that remain.
    let held = m
        .observations()
        .unwrap()
        .iter()
        .filter(|o| o.payload_held)
        .count();
    assert_eq!(held as u64, mid.payload_records);
    let done = m.compact().unwrap();
    assert!(done.resumed);
    assert_eq!(done.payloads_purged, 600 - 256);
    assert_eq!(m.stats().unwrap().payload_records, 0);
    assert_eq!(m.reconstruct().unwrap().session.compactions, 1);
}

#[test]
fn a_failure_before_reclamation_is_reported_and_the_session_is_not_marked_compacted() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    let err = m.compact_with(Inject::BeforeReclaim).unwrap_err();
    assert!(
        matches!(
            err,
            SessionMemoryError::Compaction {
                phase: chip_session_memory::CompactionPhase::Reclaim,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(
        m.reconstruct().unwrap().session.compactions,
        0,
        "not silently marked compacted"
    );
    // The purge itself is done and recorded; a rerun reclaims and then, only then, marks it.
    let done = m.compact().unwrap();
    assert!(done.nothing_to_purge);
    assert!(done.reclaim.revisions_collected > 0);
    assert_eq!(m.reconstruct().unwrap().session.compactions, 1);
}

#[test]
fn a_payload_that_does_not_match_its_digest_stops_compaction_before_anything_is_deleted() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    m.close();
    // Corrupt one stored payload behind the adapter's back.
    let db = FeltDb::open(root.path().join("s/session.felt")).unwrap();
    let mut p = db.get_value("payload:s:o007").unwrap().unwrap();
    p["text"] = serde_json::json!("not what was recorded");
    db.update("payload:s:o007", p).unwrap();
    drop(db);
    let (mut m, _) = SessionMemory::recover(root.path(), "s").unwrap();
    let err = m.compact().unwrap_err();
    assert!(
        matches!(
            err,
            SessionMemoryError::Compaction {
                phase: chip_session_memory::CompactionPhase::Preserve,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(
        m.stats().unwrap().payload_records,
        41,
        "nothing was deleted"
    );
    assert_eq!(m.reconstruct().unwrap().session.compactions, 0);
}

#[test]
fn deleting_is_not_reclaiming_and_the_two_are_reported_separately() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    let before = m.stats().unwrap();
    // Purge only: payloads are logically absent, but FeltDB still holds them.
    m.compact_with(Inject::BeforeReclaim).unwrap_err();
    let deleted_only = m.stats().unwrap();
    assert_eq!(
        deleted_only.payload_records, 2,
        "logically absent, apart from the two pinned payloads"
    );
    assert!(
        deleted_only.journal_bytes >= before.journal_bytes,
        "the journal did not shrink by deleting: {} -> {}",
        before.journal_bytes,
        deleted_only.journal_bytes
    );
    assert!(
        deleted_only.live_rows["state"] >= before.live_rows["state"],
        "revision rows still hold the deleted payloads"
    );
    // Reclaim: revisions collected, operation log pruned, journal rewritten.
    let report = m.reclaim().unwrap();
    assert!(report.revisions_collected > 0);
    assert!(report.operations_pruned > 0);
    assert!(
        report.journal_after < report.journal_before / 4,
        "{report:?}"
    );
    assert_eq!(
        m.stats()
            .unwrap()
            .live_rows
            .get("state")
            .copied()
            .unwrap_or(0),
        0
    );
}

#[test]
fn reclamation_does_not_touch_recovery_state() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    let before = recovery_view(&m.reconstruct().unwrap());
    m.reclaim().unwrap();
    m.reclaim().unwrap(); // repeatable
    assert_eq!(before, recovery_view(&m.reconstruct().unwrap()));
    assert_eq!(
        m.stats().unwrap().payload_records,
        41,
        "reclaim alone purges nothing"
    );
}

#[test]
fn a_hypothesis_that_is_still_open_keeps_the_payload_it_cites() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    m.compact().unwrap();
    // h1 is open and cites o020; h2 was resolved and cited o021.
    assert!(
        m.payload("o020").unwrap().is_some(),
        "an open hypothesis's evidence is kept"
    );
    assert!(
        m.payload("o021").unwrap().is_none(),
        "a resolved hypothesis's evidence is transient"
    );
    assert!(
        m.payload("o030").unwrap().is_some(),
        "an unresolved failure's diagnostic is kept"
    );
    assert!(
        m.payload("o010").unwrap().is_none(),
        "a superseded reading's diagnostic is transient"
    );
}

#[test]
fn interrupted_after_the_last_payload_is_gone_but_before_the_outcome_is_recorded() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    let err = m.compact_with(Inject::BeforeRecord).unwrap_err();
    assert!(
        matches!(
            err,
            SessionMemoryError::Compaction {
                phase: chip_session_memory::CompactionPhase::Record,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(
        m.stats().unwrap().payload_records,
        2,
        "the purge itself completed"
    );
    assert_eq!(
        m.reconstruct().unwrap().pending_compaction,
        Some(1),
        "but the outcome was not recorded, so it is still pending"
    );
    m.close();
    let (mut m, _) = SessionMemory::recover(root.path(), "s").unwrap();
    let done = m.compact().unwrap();
    assert!(
        done.resumed,
        "the rerun recognises the interrupted compaction even with nothing left to delete"
    );
    let rec = m.reconstruct().unwrap();
    assert_eq!(rec.pending_compaction, None);
    assert_eq!(rec.session.compactions, 1);
}

#[test]
fn an_orphan_payload_left_by_an_interrupted_write_is_purged_and_loses_nothing() {
    let root = tempfile::tempdir().unwrap();
    let mut m = rich(root.path(), "s");
    m.close();
    // A crash between the payload write and its record's write leaves a payload nobody claims.
    let db = FeltDb::open(root.path().join("s/session.felt")).unwrap();
    db.insert("payload:s:orphan", serde_json::json!({"schema": 1, "session": "s", "id": "orphan", "text": big("orphan", 5000)})).unwrap();
    drop(db);
    let (mut m, rec) = SessionMemory::recover(root.path(), "s").unwrap();
    assert_eq!(rec.observations, 41, "recovery is unaffected by the orphan");
    assert_eq!(m.stats().unwrap().payload_records, 42);
    let before = recovery_view(&rec);
    m.compact().unwrap();
    assert_eq!(m.payload("orphan").unwrap(), None);
    assert_eq!(m.stats().unwrap().payload_records, 2);
    assert_eq!(before, recovery_view(&m.reconstruct().unwrap()));
}
