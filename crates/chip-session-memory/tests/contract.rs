//! One behavioral contract, run unchanged against every storage candidate.
//!
//! `contract!(name, Backend)` instantiates the same tests for FeltDB, SQLite and redb. The shared
//! logic (schema, compaction, recovery) is the same code in all three; what differs is only the
//! engine underneath, so these tests decide whether an engine can carry the recovery contract:
//! survive reopening, keep recovery-critical information through compaction, survive interrupted
//! compaction, refuse damaged or inconsistent data by name, and recover after a real SIGKILL.
//!
//! Process-kill tests establish survival of the *process*, not of power: see the report.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chip_session_memory::compaction::Inject;
use chip_session_memory::packet::packet_and_hash;
use chip_session_memory::*;
use common::*;

const CHILD: &str = "CONTRACT_CHILD";

/// A view of a recovered session that compares what matters and nothing engine-specific
/// (a checkpoint's sequence and digest are engine-defined).
fn view(rec: &RecoveredSession) -> String {
    let mut r = rec.clone();
    if let Some(c) = r.checkpoint.as_mut() {
        c.sequence = 0;
        c.state_digest.clear();
    }
    format!("{r:#?}")
}

fn dir_of(root: &Path, s: &str) -> PathBuf {
    root.join(s)
}

// ----------------------------------------------------------------------- the child processes

fn child_body<B: Backend>(root: &Path, mode: &str) {
    let mut m = SessionMemory::<B>::create_with(root, "k", "survive being killed").unwrap();
    for i in 0..50 {
        m.record_observation(obs(&format!("o{i:03}"), "read", &big("p", 4000)), true)
            .unwrap();
    }
    m.record_attempt("a1", "first try", Outcome::Failed, vec![r("obs", "o001")])
        .unwrap();
    match mode {
        "writes" => {
            println!("READY");
            std::io::stdout().flush().unwrap();
        }
        "after_checkpoint" => {
            m.checkpoint("c1").unwrap();
            println!("READY");
            std::io::stdout().flush().unwrap();
        }
        "compact" => {
            for i in 50..1500 {
                m.record_observation(obs(&format!("o{i:04}"), "read", &big("q", 2000)), true)
                    .unwrap();
            }
            m.checkpoint("c1").unwrap();
            println!("READY");
            std::io::stdout().flush().unwrap();
            loop {
                m.compact().unwrap();
            }
        }
        other => panic!("unknown child mode {other}"),
    }
    let mut i = 50;
    loop {
        m.record_observation(obs(&format!("w{i:05}"), "read", &big("p", 4000)), true)
            .unwrap();
        i += 1;
    }
}

/// The child half of the kill tests. In a normal run this does nothing.
#[test]
fn child_entry() {
    let Ok(spec) = std::env::var(CHILD) else {
        return;
    };
    if std::env::var_os("CONTRACT_SYNC").is_some() {
        chip_session_memory::backend::SYNC_EVERY_WRITE
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let mut it = spec.splitn(3, ':');
    let (backend, mode, root) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
    let root = Path::new(root);
    match backend {
        "feltdb" => child_body::<Felt>(root, mode),
        "sqlite" => child_body::<Sqlite>(root, mode),
        "redb" => child_body::<Redb>(root, mode),
        other => panic!("unknown backend {other}"),
    }
}

/// Runs a child until it reports READY, lets it run for `delay_ms`, then SIGKILLs it.
fn kill_child(backend: &str, mode: &str, root: &Path, delay_ms: u64) {
    kill_child_with(backend, mode, root, delay_ms, false)
}

fn kill_child_with(backend: &str, mode: &str, root: &Path, delay_ms: u64, sync: bool) {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    if sync {
        cmd.env("CONTRACT_SYNC", "1");
    }
    let mut child = cmd
        .args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env(CHILD, format!("{backend}:{mode}:{}", root.display()))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
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
    if rx
        .recv_timeout(std::time::Duration::from_secs(120))
        .is_err()
    {
        let _ = child.kill();
        panic!("the child never reported READY");
    }
    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success(), "the child was killed");
}

// ------------------------------------------------------------------------------ the contract

macro_rules! contract {
    ($name:ident, $backend:ty, $label:literal) => {
        mod $name {
            use super::*;
            type B = $backend;
            type M = SessionMemory<$backend>;

            #[test]
            fn normal_close_and_reopen_reconstructs_the_same_session() {
                let root = tempfile::tempdir().unwrap();
                let mut m = rich_in::<B>(root.path(), "s");
                m.checkpoint("c1").unwrap();
                let live = m.reconstruct().unwrap();
                let (live_packet, live_hash) = packet_and_hash(&m, &live);
                m.close();
                drop(m);
                let (m2, rec) = M::recover_with(root.path(), "s").unwrap();
                assert_eq!(view(&rec), view(&live));
                assert_eq!(packet_and_hash(&m2, &rec), (live_packet, live_hash));
                assert_eq!(rec.checkpoint.as_ref().unwrap().id, "c1");
                assert_eq!(rec.unresolved_failures[0].command, "integration");
                assert_eq!(rec.open_hypotheses[0].id, "h1");
                assert!(rec.session.pending.as_ref().unwrap().decision_needed);
            }

            #[test]
            fn compaction_drops_eligible_payloads_and_keeps_what_recovery_needs() {
                let root = tempfile::tempdir().unwrap();
                let mut m = rich_in::<B>(root.path(), "s");
                m.checkpoint("c1").unwrap();
                let before = m.reconstruct().unwrap();
                let held_before = m.stats().unwrap().payload_records;
                let report = m.compact().unwrap();
                assert!(report.payloads_purged > 0);
                let after = m.reconstruct().unwrap();
                // Everything except the count of payloads still held is unchanged.
                let strip = |r: &RecoveredSession| {
                    let mut r = r.clone();
                    r.payloads_held = 0;
                    r.session.compactions = 0;
                    view(&r)
                };
                assert_eq!(strip(&before), strip(&after));
                assert_eq!(after.session.compactions, 1);
                // Only pinned payloads remain: the unresolved failure's diagnostic and the open
                // hypothesis' evidence (o020 for h1, o030 for tr4).
                let left = m.stats().unwrap().payload_records;
                assert_eq!(left, 2, "held before: {held_before}");
                assert!(m.payload("o030").unwrap().is_some());
                assert!(m.payload("o020").unwrap().is_some());
                assert!(m.payload("o001").unwrap().is_none());
                // Idempotent.
                let again = m.compact().unwrap();
                assert!(again.nothing_to_purge);
                assert_eq!(m.reconstruct().unwrap().session.compactions, 1);
            }

            #[test]
            fn the_recovery_packet_survives_compaction_and_reopening_unchanged() {
                let root = tempfile::tempdir().unwrap();
                let mut m = rich_in::<B>(root.path(), "s");
                m.checkpoint("c1").unwrap();
                let rec = m.reconstruct().unwrap();
                let (_, h0) = packet_and_hash(&m, &rec);
                m.compact().unwrap();
                let rec = m.reconstruct().unwrap();
                let (_, h1) = packet_and_hash(&m, &rec);
                assert_eq!(h0, h1, "compaction changed the recovery packet");
                m.close();
                drop(m);
                let (m2, rec) = M::recover_with(root.path(), "s").unwrap();
                let (_, h2) = packet_and_hash(&m2, &rec);
                assert_eq!(h0, h2, "reopening changed the recovery packet");
            }

            #[test]
            fn compaction_interrupted_at_every_seam_loses_nothing_and_a_rerun_completes() {
                // Reference: an uninterrupted compaction of the same session.
                let build = |root: &Path| -> M {
                    let mut m = rich_in::<B>(root, "s");
                    // Enough small payloads to span several purge batches (256 per batch).
                    for i in 0..600 {
                        m.record_observation(
                            obs(&format!("b{i:04}"), "bulk", &big("b", 300)),
                            true,
                        )
                        .unwrap();
                    }
                    m.checkpoint("c1").unwrap();
                    m
                };
                let ref_root = tempfile::tempdir().unwrap();
                let mut reference = build(ref_root.path());
                reference.compact().unwrap();
                let want = reference.reconstruct().unwrap();
                let (_, want_hash) = packet_and_hash(&reference, &want);
                let want_stats = reference.stats().unwrap();

                for inject in [
                    Inject::AfterPreserve,
                    Inject::DuringPurge { batches: 1 },
                    Inject::BeforeRecord,
                    Inject::BeforeReclaim,
                ] {
                    let root = tempfile::tempdir().unwrap();
                    let mut m = build(root.path());
                    let before = m.reconstruct().unwrap();
                    let (_, before_hash) = packet_and_hash(&m, &before);
                    let err = m.compact_with(inject).expect_err("injected interruption");
                    assert!(matches!(err, SessionMemoryError::Compaction { .. }), "{err:?}");
                    // The session is never marked compacted by an interrupted run, and recovery
                    // information is intact straight away.
                    assert_eq!(m.reconstruct().unwrap().session.compactions, 0, "{inject:?}");
                    // The process dies here: reopen from disk.
                    m.close();
                    drop(m);
                    let (mut m, rec) = M::recover_with(root.path(), "s").unwrap();
                    assert_eq!(packet_and_hash(&m, &rec).1, before_hash, "{inject:?}");
                    // A rerun resumes and finishes.
                    m.compact().unwrap();
                    let got = m.reconstruct().unwrap();
                    assert_eq!(got.session.compactions, 1, "{inject:?}");
                    assert_eq!(packet_and_hash(&m, &got).1, want_hash, "{inject:?}");
                    assert_eq!(m.stats().unwrap().records, want_stats.records, "{inject:?}");
                    assert_eq!(
                        m.stats().unwrap().payload_records,
                        want_stats.payload_records,
                        "{inject:?}"
                    );
                }
            }

            #[test]
            fn repeated_recovery_neither_duplicates_nor_changes_anything() {
                let root = tempfile::tempdir().unwrap();
                let mut m = rich_in::<B>(root.path(), "s");
                m.checkpoint("c1").unwrap();
                m.compact().unwrap();
                let first = m.reconstruct().unwrap();
                let stats = m.stats().unwrap();
                m.close();
                drop(m);
                for round in 0..4 {
                    let (mut m, rec) = M::recover_with(root.path(), "s").unwrap();
                    assert_eq!(view(&rec), view(&first), "round {round}");
                    let now = m.stats().unwrap();
                    assert_eq!(now.records, stats.records, "round {round}");
                    assert_eq!(now.payload_records, stats.payload_records, "round {round}");
                    assert!(m.compact().unwrap().nothing_to_purge);
                    m.close();
                }
                // Work after recovery adds exactly what it writes.
                let (mut m, _) = M::recover_with(root.path(), "s").unwrap();
                m.record_observation(obs("post", "after recovery", &big("x", 100)), true)
                    .unwrap();
                let now = m.stats().unwrap();
                assert_eq!(now.records["obs"], stats.records["obs"] + 1);
            }

            #[test]
            fn a_process_killed_during_writes_leaves_a_session_that_recovers_consistently() {
                // No checkpoint was taken after creation: the contract is a consistent session,
                // not a particular amount of progress.
                let root = tempfile::tempdir().unwrap();
                kill_child($label, "writes", root.path(), 150);
                let (mut m, rec) = M::recover_with(root.path(), "k").expect("recovery after SIGKILL");
                assert_eq!(rec.session.objective, "survive being killed");
                for o in m.observations().unwrap() {
                    if o.payload_held {
                        assert!(
                            m.payload(&o.id).unwrap().is_some(),
                            "{} claims a payload that is not stored",
                            o.id
                        );
                    }
                }
                let n = m.observations().unwrap().len();
                let mut ids: Vec<_> = m.observations().unwrap().into_iter().map(|o| o.id).collect();
                ids.dedup();
                assert_eq!(ids.len(), n, "no duplicated records");
                // Report what survived; the number is informational, the guarantee is consistency.
                println!("SURVIVED backend={} observations={n}", $label);
                m.compact().unwrap();
                m.checkpoint("c2").unwrap();
                assert_eq!(m.stats().unwrap().payload_records, 0);
            }

            #[test]
            fn with_every_write_synced_a_killed_process_loses_no_acknowledged_write() {
                let root = tempfile::tempdir().unwrap();
                kill_child_with($label, "writes", root.path(), 100, true);
                let (m, rec) = M::recover_with(root.path(), "k").expect("recovery after SIGKILL");
                // Everything acknowledged before READY (50 observations and an attempt) is there,
                // with no checkpoint to lean on.
                assert!(rec.observations >= 50, "{}", rec.observations);
                for i in 0..50 {
                    assert!(m.observation(&format!("o{i:03}")).unwrap().is_some(), "o{i:03}");
                }
                assert_eq!(rec.failed_approaches[0].action, "first try");
            }

            #[test]
            fn a_process_killed_after_a_committed_checkpoint_keeps_everything_up_to_it() {
                let root = tempfile::tempdir().unwrap();
                kill_child($label, "after_checkpoint", root.path(), 150);
                let (mut m, rec) = M::recover_with(root.path(), "k").expect("recovery after SIGKILL");
                assert_eq!(rec.checkpoint.as_ref().unwrap().id, "c1");
                assert_eq!(rec.failed_approaches[0].action, "first try");
                assert!(rec.observations >= 50, "{}", rec.observations);
                for i in 0..50 {
                    assert!(m.observation(&format!("o{i:03}")).unwrap().is_some(), "o{i:03}");
                }
                m.compact().unwrap();
                m.checkpoint("c2").unwrap();
            }

            #[test]
            fn a_process_killed_during_compaction_recovers_and_a_rerun_completes() {
                for delay in [0u64, 15, 60] {
                    let root = tempfile::tempdir().unwrap();
                    kill_child($label, "compact", root.path(), delay);
                    let (mut m, rec) =
                        M::recover_with(root.path(), "k").expect("recovery after SIGKILL");
                    assert_eq!(rec.checkpoint.as_ref().unwrap().id, "c1", "delay {delay}");
                    for i in 0..50 {
                        assert!(m.observation(&format!("o{i:03}")).unwrap().is_some());
                    }
                    m.compact().unwrap();
                    let after = m.reconstruct().unwrap();
                    assert!(after.session.compactions >= 1, "delay {delay}");
                    assert_eq!(m.stats().unwrap().payload_records, 0, "delay {delay}");
                    assert!(m.compact().unwrap().nothing_to_purge);
                }
            }

            fn tampered(tamper: impl FnOnce(&B)) -> std::result::Result<(), SessionMemoryError> {
                let root = tempfile::tempdir().unwrap();
                let mut m = rich_in::<B>(root.path(), "s");
                m.checkpoint("c1").unwrap();
                m.compact().unwrap();
                m.checkpoint("c1").unwrap();
                m.close();
                drop(m);
                {
                    let b = B::open(&dir_of(root.path(), "s"), "s").unwrap();
                    tamper(&b);
                }
                M::recover_with(root.path(), "s").map(|_| ())
            }

            fn refused(name: &str, expect: &str, tamper: impl FnOnce(&B)) {
                match tampered(tamper) {
                    Err(SessionMemoryError::Recovery(msg)) => {
                        assert!(msg.contains(expect), "{name}: {msg}")
                    }
                    other => panic!("{name}: expected a recovery refusal, got {other:?}"),
                }
            }

            #[test]
            fn missing_corrupt_and_inconsistent_records_are_refused_by_name() {
                refused("a checkpoint reference is missing", "checkpoint c1 references missing task t1", |b| {
                    b.delete("task", "t1").unwrap()
                });
                refused("an escalation cites a missing attempt", "missing attempt a1", |b| {
                    b.delete("attempt", "a1").unwrap()
                });
                refused("a repair is verified by a missing result", "missing test result ghost", |b| {
                    let mut v = b.get("repair", "rp1").unwrap().unwrap();
                    v["verified_by"] = serde_json::json!("ghost");
                    b.put("repair", "rp1", &v).unwrap()
                });
                refused("a repair is verified by a failing result", "did not pass", |b| {
                    let mut v = b.get("repair", "rp1").unwrap().unwrap();
                    v["verified_by"] = serde_json::json!("tr1");
                    b.put("repair", "rp1", &v).unwrap()
                });
                refused("a test result cites a missing observation", "test result tr3 cites missing observation o012", |b| {
                    b.delete("obs", "o012").unwrap()
                });
                refused("a record belongs to another session", "other", |b| {
                    b.put("attempt", "intruder", &serde_json::json!({"schema": 1, "session": "other", "id": "intruder", "action": "x", "outcome": "failed", "refs": []})).unwrap()
                });
                refused("the checkpoint record is gone", "checkpoint c1 is missing", |b| {
                    b.delete("ckpt", "c1").unwrap()
                });
                refused("a pinned payload is gone", "lost its payload", |b| {
                    b.delete("payload", "o030").unwrap()
                });
                refused("a record is not a valid record", "query failed", |b| {
                    b.put("attempt", "a1", &serde_json::json!({"session": "s", "garbage": true})).unwrap()
                });
                refused("the session record is gone", "session record is missing", |b| {
                    b.delete("session", "meta").unwrap()
                });
            }

            #[test]
            fn a_damaged_storage_file_is_refused_and_not_modified() {
                let root = tempfile::tempdir().unwrap();
                let mut m = rich_in::<B>(root.path(), "s");
                m.checkpoint("c1").unwrap();
                m.close();
                drop(m);
                let path = dir_of(root.path(), "s").join(B::FILE);
                // Overwrite the head of the file with text: no engine may treat that as a store.
                let mut bytes = std::fs::read(&path).unwrap();
                let n = bytes.len().min(4096);
                for (i, b) in bytes[..n].iter_mut().enumerate() {
                    *b = b"not a database "[i % 15];
                }
                std::fs::write(&path, &bytes).unwrap();
                let r = M::recover_with(root.path(), "s");
                assert!(
                    matches!(r, Err(SessionMemoryError::Recovery(_))),
                    "{:?}",
                    r.err()
                );
                assert_eq!(std::fs::read(&path).unwrap(), bytes, "a refused open modifies nothing");
            }

            /// Not a pass/fail comparison: a single flipped byte at five places in the store file,
            /// classified as refused (the engine or the adapter noticed), changed (recovered but
            /// the data differs from what was written, i.e. silent corruption) or unaffected.
            /// The only hard assertion is that nothing panics. The counts go into the report.
            #[test]
            fn a_single_flipped_byte_is_classified_not_assumed() {
                let (mut refused, mut changed, mut unaffected) = (0, 0, 0);
                for pct in [10usize, 30, 50, 70, 90] {
                    let root = tempfile::tempdir().unwrap();
                    let mut m = rich_in::<B>(root.path(), "s");
                    m.checkpoint("c1").unwrap();
                    let rec = m.reconstruct().unwrap();
                    let (_, want) = packet_and_hash(&m, &rec);
                    let payloads: Vec<(String, Option<String>)> = m
                        .observations()
                        .unwrap()
                        .into_iter()
                        .map(|o| (o.id.clone(), m.payload(&o.id).unwrap()))
                        .collect();
                    m.close();
                    drop(m);
                    let path = dir_of(root.path(), "s").join(B::FILE);
                    let mut bytes = std::fs::read(&path).unwrap();
                    // If the engine left a WAL, the data may live there; flip in the main file
                    // after a clean close, which is where a checkpointed engine keeps it.
                    let at = bytes.len() * pct / 100;
                    bytes[at] ^= 0x20;
                    std::fs::write(&path, &bytes).unwrap();
                    let outcome = match M::recover_with(root.path(), "s") {
                        Err(_) => "refused",
                        Ok((m, rec)) => {
                            let same_packet = packet_and_hash(&m, &rec).1 == want;
                            let same_payloads = payloads
                                .iter()
                                .all(|(id, p)| m.payload(id).ok().as_ref() == Some(p));
                            if same_packet && same_payloads { "unaffected" } else { "changed" }
                        }
                    };
                    match outcome {
                        "refused" => refused += 1,
                        "changed" => changed += 1,
                        _ => unaffected += 1,
                    }
                }
                println!(
                    "BITFLIP backend={} refused={refused} changed={changed} unaffected={unaffected}",
                    $label
                );
            }

            #[test]
            fn a_missing_session_is_refused() {
                let root = tempfile::tempdir().unwrap();
                assert!(matches!(
                    M::recover_with(root.path(), "never"),
                    Err(SessionMemoryError::Recovery(_))
                ));
                assert!(matches!(
                    M::recover_with(root.path(), "../etc"),
                    Err(SessionMemoryError::Invalid(_))
                ));
            }

            #[test]
            fn sessions_are_isolated_and_destroy_removes_only_its_own_files() {
                let root = tempfile::tempdir().unwrap();
                let a = rich_in::<B>(root.path(), "a");
                let mut b = rich_in::<B>(root.path(), "b");
                b.record_observation(obs("only-b", "b", "b"), true).unwrap();
                assert!(a.observation("only-b").unwrap().is_none());
                b.close();
                a.destroy().unwrap();
                assert!(!root.path().join("a").exists());
                let (_b, rec) = M::recover_with(root.path(), "b").unwrap();
                assert_eq!(rec.observations, 42);
            }

            #[test]
            fn the_engine_reports_its_own_integrity() {
                let root = tempfile::tempdir().unwrap();
                let mut m = rich_in::<B>(root.path(), "s");
                m.checkpoint("c1").unwrap();
                m.compact().unwrap();
                m.close();
                drop(m);
                let mut b = B::open(&dir_of(root.path(), "s"), "s").unwrap();
                let verdict = b.integrity().unwrap();
                assert!(verdict == "ok" || verdict.starts_with("not offered"), "{verdict}");
            }
        }
    };
}

contract!(feltdb, Felt, "feltdb");
contract!(sqlite, Sqlite, "sqlite");
contract!(redb, Redb, "redb");
