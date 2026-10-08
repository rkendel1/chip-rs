//! Memory and latency benchmark: Chip's existing in-memory path against the experimental native
//! FeltDB path, on the same deterministic workload.
//!
//! `cargo bench -p chip-session-memory --bench session_memory [-- --observations N --trials N
//! --seed N --quick --out PATH]`. Writes `target/session-memory-bench.json` (units, configuration,
//! environment and method included) and prints a summary.
//!
//! **Method.** Every arm and trial runs in its own *child process* (this binary re-executed with
//! `--child`), because a process's heap never shrinks back to its start and one arm's allocations
//! would otherwise contaminate the next. Trials of different arms are interleaved. Memory is
//! reported at named points as five distinct things:
//!
//! * *logical bytes*: what the data is (payload and record sizes), not what it costs;
//! * *live rows*: records FeltDB itself reports holding (revision rows included);
//! * *heap in use* and *heap reserved*: glibc `mallinfo2` for the main arena plus mmapped chunks.
//!   The workload runs on the main thread, so this is the whole allocation picture;
//! * *RSS*: `/proc/self/status` `VmRSS`, and `VmHWM` for the peak;
//! * *returned to the OS*: RSS after an explicit `malloc_trim(0)`, taken as a separate step so the
//!   allocator's own behaviour is never credited to (or blamed on) the storage layer.
//!
//! Peak RSS is `VmHWM`, which only rises; "peak during compaction" is therefore reported as the
//! high-water mark after compaction and whether it exceeded the high-water mark before it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use chip_core::{
    CapabilityId, ExecutionId, ExecutionStatus, Observation, ObservationKind, ObservationOrigin,
    omissions,
};
use chip_session_memory::workload::{Event, Workload};
use chip_session_memory::{Outcome, SessionMemory};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const ARMS: [&str; 4] = [
    "chip_baseline",
    "memory_compact",
    "felt_store_all",
    "felt_summaries_only",
];
const FELTDB_REV: &str = "9f2354e89743bf1bdc8f1fc825d8259fd80920fe";

// ------------------------------------------------------------------------------ measurement

#[repr(C)]
struct Mallinfo2 {
    arena: usize,
    ordblks: usize,
    smblks: usize,
    hblks: usize,
    hblkhd: usize,
    usmblks: usize,
    fsmblks: usize,
    uordblks: usize,
    fordblks: usize,
    keepcost: usize,
}
unsafe extern "C" {
    fn mallinfo2() -> Mallinfo2;
    fn malloc_trim(pad: usize) -> i32;
}

fn status_kib(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix(field).map(str::to_string))
        })
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

fn threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .map(|d| d.count())
        .unwrap_or(0)
}

/// One named memory reading. Bytes unless the key says KiB.
fn mem() -> Value {
    // SAFETY: mallinfo2 only reads allocator state and has no preconditions.
    let m = unsafe { mallinfo2() };
    json!({
        "rss_kib": status_kib("VmRSS:"),
        "hwm_kib": status_kib("VmHWM:"),
        "heap_in_use_bytes": m.uordblks + m.hblkhd,
        "heap_free_in_arena_bytes": m.fordblks,
        "heap_reserved_bytes": m.arena + m.hblkhd,
    })
}

fn trim() {
    // SAFETY: malloc_trim is always safe to call.
    unsafe { malloc_trim(0) };
}

struct Latencies(Vec<f64>);
impl Latencies {
    fn new() -> Self {
        Self(Vec::new())
    }
    fn record(&mut self, started: Instant) {
        self.0.push(started.elapsed().as_secs_f64() * 1e6);
    }
    fn summary(&mut self) -> Value {
        if self.0.is_empty() {
            return Value::Null;
        }
        self.0.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = self.0.len();
        let at = |q: f64| self.0[((n as f64 - 1.0) * q) as usize];
        json!({"count": n, "p50_us": at(0.5), "p99_us": at(0.99), "mean_us": self.0.iter().sum::<f64>() / n as f64, "max_us": self.0[n - 1]})
    }
}

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1e3
}

fn sha(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ------------------------------------------------------------------------------ the expected state

/// The small facts a session is made of, tracked independently of any storage so that every arm's
/// recovered state can be checked against the same expectation.
#[derive(Default)]
struct Facts {
    failed: Vec<(String, String)>,
    verified: Vec<(String, String)>,
    tests: Vec<(String, String, bool, Vec<String>, String)>, // id, command, passed, failed tests, diagnostic
    hypotheses: Vec<(String, bool, Vec<String>)>,            // id, open, evidence
    escalations: Vec<(String, String, Vec<String>)>,
    plan: Option<Vec<String>>,
    plan_revisions: u32,
    pending: Option<String>,
    checkpoint: Option<String>,
    observations: u64,
}

impl Facts {
    fn note(&mut self, e: &Event) {
        match e {
            Event::Plan { steps } => {
                self.plan = Some(steps.clone());
                self.plan_revisions += 1;
            }
            Event::Observation { .. } => self.observations += 1,
            Event::Attempt {
                id,
                action,
                outcome,
                ..
            } if *outcome == Outcome::Failed => self.failed.push((id.clone(), action.clone())),
            Event::TestResult {
                id,
                command,
                passed,
                failed_tests,
                diagnostic,
            } => self.tests.push((
                id.clone(),
                command.clone(),
                *passed,
                failed_tests.clone(),
                diagnostic.clone(),
            )),
            Event::Repair {
                id,
                outcome,
                verified_by: Some(v),
                ..
            } if *outcome == Outcome::Succeeded => self.verified.push((id.clone(), v.clone())),
            Event::Hypothesis { id, evidence, .. } => self.hypotheses.push((
                id.clone(),
                true,
                evidence.iter().map(|r| r.id.clone()).collect(),
            )),
            Event::ResolveHypothesis { id } => {
                if let Some(h) = self.hypotheses.iter_mut().find(|h| &h.0 == id) {
                    h.1 = false;
                }
            }
            Event::Escalation {
                id,
                reason,
                outstanding,
                ..
            } => self
                .escalations
                .push((id.clone(), reason.clone(), outstanding.clone())),
            Event::Pending(p) => self.pending = Some(p.description.clone()),
            Event::Checkpoint { id } => self.checkpoint = Some(id.clone()),
            _ => {}
        }
    }

    /// The current reading of each command, when it is failing.
    fn unresolved(&self) -> Vec<(String, Vec<String>, String)> {
        let mut latest: BTreeMap<&str, &(String, String, bool, Vec<String>, String)> =
            BTreeMap::new();
        for t in &self.tests {
            latest.insert(&t.1, t);
        }
        latest
            .values()
            .filter(|t| !t.2)
            .map(|t| (t.1.clone(), t.3.clone(), t.4.clone()))
            .collect()
    }

    fn pinned(&self) -> Vec<String> {
        let mut p: Vec<String> = self.unresolved().into_iter().map(|u| u.2).collect();
        for h in self.hypotheses.iter().filter(|h| h.1) {
            p.extend(h.2.iter().cloned());
        }
        p.sort();
        p.dedup();
        p
    }
}

/// The canonical packet an escalation or a recovery needs. Identical across arms or the arm
/// failed to preserve something.
fn packet(
    objective: &str,
    status: &str,
    plan: Option<&Vec<String>>,
    plan_revisions: u32,
    failed: &[(String, String)],
    verified: &[(String, String)],
    unresolved: &[(String, Vec<String>, String, String)],
    open: &[String],
    escalations: &[(String, String, Vec<String>)],
    checkpoint: Option<&String>,
    pending: Option<&String>,
    observations: u64,
) -> Value {
    let mut failed = failed.to_vec();
    failed.sort();
    let mut verified = verified.to_vec();
    verified.sort();
    let mut open = open.to_vec();
    open.sort();
    let mut esc = escalations.to_vec();
    esc.sort();
    json!({
        "objective": objective, "status": status, "plan": plan, "plan_revisions": plan_revisions,
        "failed_approaches": failed, "verified_repairs": verified,
        "unresolved_failures": unresolved, "open_hypotheses": open, "escalations": esc,
        "checkpoint": checkpoint, "pending": pending, "observations": observations,
    })
}

fn expected_packet(w: &Workload, f: &Facts, diag_sha: impl Fn(&str) -> String) -> Value {
    let unresolved: Vec<_> = f
        .unresolved()
        .into_iter()
        .map(|(c, t, d)| {
            let s = diag_sha(&d);
            (c, t, d, s)
        })
        .collect();
    let open: Vec<String> = f
        .hypotheses
        .iter()
        .filter(|h| h.1)
        .map(|h| h.0.clone())
        .collect();
    packet(
        &w.objective(),
        if f.escalations.is_empty() {
            "active"
        } else {
            "escalated"
        },
        f.plan.as_ref(),
        f.plan_revisions,
        &f.failed,
        &f.verified,
        &unresolved,
        &open,
        &f.escalations,
        f.checkpoint.as_ref(),
        f.pending.as_ref(),
        f.observations,
    )
}

// ------------------------------------------------------------------------------ the arms

fn arm_chip_baseline(w: &Workload) -> Value {
    let mut out = json!({"arm": "chip_baseline"});
    out["threads_start"] = json!(threads());
    out["memory_start"] = mem();
    let mut facts = Facts::default();
    let mut observations: Vec<Observation> = Vec::new();
    let mut origins: Vec<ObservationOrigin> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut write = Latencies::new();
    let started = Instant::now();
    for e in w.events() {
        let op = Instant::now();
        facts.note(&e);
        if let Event::Observation {
            id,
            kind,
            provenance,
            payload,
            repeats,
            ..
        } = e
        {
            // The structures Chip holds for a run: the full observation and where it came from.
            let invocation = format!("{kind}:{}", repeats.as_deref().unwrap_or(&id));
            origins.push(ObservationOrigin {
                capability: CapabilityId::new("project.read").unwrap(),
                invocation,
                reusable: kind != "test" && kind != "git",
                provider_response_id: None,
            });
            index.insert(id, observations.len());
            observations.push(Observation {
                execution_id: ExecutionId::new(provenance),
                kind: ObservationKind::ExecutionCompleted,
                status: ExecutionStatus::Success,
                output: Some(payload),
                receipt_id: None,
                evidence: None,
            });
        }
        write.record(op);
    }
    out["timing_ms"] = json!({"ingest_total": ms(started)});
    out["latency_us"] = json!({"write": write.summary()});
    out["memory_after_ingest"] = mem();
    let logical: u64 = observations
        .iter()
        .map(|o| o.output.as_ref().map_or(0, |t| t.len() as u64))
        .sum();

    // Reads and queries over what it holds.
    let ids: Vec<String> = index.keys().cloned().collect();
    let mut read = Latencies::new();
    for i in 0..2000usize {
        let id = &ids[(i * 7919) % ids.len()];
        let op = Instant::now();
        let _ = observations[index[id]].output.as_ref().map(|t| t.len());
        read.record(op);
    }
    let q = Instant::now();
    let tests = observations
        .iter()
        .filter(|o| o.output.as_ref().is_some_and(|t| t.starts_with("error")))
        .count();
    out["latency_us"]["read"] = read.summary();
    out["timing_ms"]["full_scan_query"] = json!(ms(q));
    let _ = tests;

    // What Chip would hand a model on escalation today: every observation not omitted as a
    // repeat, in full (the DeduplicatedEscalationContext rule, by Chip's own function).
    let t = Instant::now();
    let left_out: std::collections::HashSet<usize> = omissions(&origins, &observations)
        .into_iter()
        .map(|(i, _)| i)
        .collect();
    let mut context_bytes = 0usize;
    let mut context_count = 0usize;
    for (i, o) in observations.iter().enumerate() {
        if !left_out.contains(&i) {
            context_bytes += o.render().len();
            context_count += 1;
        }
    }
    out["timing_ms"]["context_build"] = json!(ms(t));
    out["memory_after_context"] = mem();

    let diag = |id: &str| {
        observations[index[id]]
            .output
            .as_deref()
            .map(sha)
            .unwrap_or_default()
    };
    let pk = expected_packet(w, &facts, diag);
    let pk_text = pk.to_string();
    out["recovery"] = json!({"supported": false, "why": "Chip keeps a run in memory only; there is no persisted form to recover from", "packet_sha256": sha(&pk_text), "packet_bytes": pk_text.len(), "packet": pk, "escalation_context_bytes_today": context_bytes, "escalation_context_observations": context_count, "observations_omitted_as_repeats": left_out.len()});
    out["logical"] = json!({"records_before": observations.len(), "records_after": observations.len(), "payload_bytes_before": logical, "payload_bytes_after": logical, "purge_candidate_records": 0, "purge_candidate_bytes": 0, "note": "no compaction exists in the current path"});
    drop(observations);
    drop(origins);
    drop(index);
    out["memory_after_destroy"] = mem();
    trim();
    out["memory_after_destroy_and_trim"] = mem();
    out["threads_end"] = json!(threads());
    out
}

/// A plain in-memory store-then-compact session: what compaction alone achieves with no FeltDB.
fn arm_memory_compact(w: &Workload) -> Value {
    struct Rec {
        id: String,
        kind: String,
        summary: String,
        digest: String,
        len: u64,
        excerpt: String,
        provenance: String,
        payload: Option<String>,
    }
    let mut out = json!({"arm": "memory_compact"});
    out["threads_start"] = json!(threads());
    out["memory_start"] = mem();
    let mut facts = Facts::default();
    let mut recs: Vec<Rec> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut write = Latencies::new();
    let started = Instant::now();
    for e in w.events() {
        let op = Instant::now();
        facts.note(&e);
        if let Event::Observation {
            id,
            kind,
            provenance,
            summary,
            payload,
            ..
        } = e
        {
            let digest = sha(&payload);
            let excerpt = payload.chars().take(256).collect();
            index.insert(id.clone(), recs.len());
            recs.push(Rec {
                id,
                kind,
                summary,
                digest,
                len: payload.len() as u64,
                excerpt,
                provenance,
                payload: Some(payload),
            });
        }
        write.record(op);
    }
    out["timing_ms"] = json!({"ingest_total": ms(started)});
    out["latency_us"] = json!({"write": write.summary()});
    out["memory_after_ingest"] = mem();
    let payload_before: u64 = recs
        .iter()
        .filter_map(|r| r.payload.as_ref())
        .map(|p| p.len() as u64)
        .sum();
    let record_bytes: u64 = recs
        .iter()
        .map(|r| {
            (r.id.len()
                + r.kind.len()
                + r.summary.len()
                + r.digest.len()
                + r.excerpt.len()
                + r.provenance.len()
                + 8) as u64
        })
        .sum();

    let ids: Vec<String> = index.keys().cloned().collect();
    let mut read = Latencies::new();
    for i in 0..2000usize {
        let id = &ids[(i * 7919) % ids.len()];
        let op = Instant::now();
        let _ = recs[index[id]].payload.as_ref().map(|t| t.len());
        read.record(op);
    }
    out["latency_us"]["read"] = read.summary();
    let q = Instant::now();
    let _ = recs.iter().filter(|r| r.kind == "test").count();
    out["timing_ms"]["full_scan_query"] = json!(ms(q));

    let hwm_before = status_kib("VmHWM:");
    let t = Instant::now();
    let pinned: std::collections::HashSet<String> = facts.pinned().into_iter().collect();
    let (mut purged, mut purged_bytes) = (0u64, 0u64);
    for r in recs.iter_mut() {
        if !pinned.contains(&r.id) {
            if let Some(p) = r.payload.take() {
                purged += 1;
                purged_bytes += p.len() as u64;
            }
        }
    }
    out["timing_ms"]["compaction"] = json!(ms(t));
    out["memory_after_compaction"] = mem();
    out["memory_peak_during_compaction"] = json!({"hwm_before_kib": hwm_before, "hwm_after_kib": status_kib("VmHWM:"), "exceeded": status_kib("VmHWM:") > hwm_before});
    trim();
    out["memory_after_compaction_and_trim"] = mem();

    let diag = |id: &str| {
        recs[index[id]]
            .payload
            .as_deref()
            .map(sha)
            .unwrap_or_default()
    };
    let pk = expected_packet(w, &facts, diag);
    let pk_text = pk.to_string();
    out["recovery"] = json!({"supported": false, "why": "in memory only: nothing survives the process", "packet_sha256": sha(&pk_text), "packet_bytes": pk_text.len(), "packet": pk});
    out["logical"] = json!({"records_before": recs.len(), "records_after": recs.len(), "payload_bytes_before": payload_before, "payload_bytes_after": payload_before - purged_bytes, "record_bytes_before": record_bytes, "record_bytes_after": record_bytes, "purge_candidate_records": purged, "purge_candidate_bytes": purged_bytes});
    drop(recs);
    drop(index);
    out["memory_after_destroy"] = mem();
    trim();
    out["memory_after_destroy_and_trim"] = mem();
    out["threads_end"] = json!(threads());
    out
}

fn felt_packet(
    w: &Workload,
    m: &SessionMemory,
    rec: &chip_session_memory::RecoveredSession,
) -> Value {
    let unresolved: Vec<_> = rec
        .unresolved_failures
        .iter()
        .map(|t| {
            let d = t.diagnostic.clone().unwrap_or_default();
            let s = m
                .payload(&d)
                .ok()
                .flatten()
                .as_deref()
                .map(sha)
                .unwrap_or_default();
            (t.command.clone(), t.failed_tests.clone(), d, s)
        })
        .collect();
    let failed: Vec<_> = rec
        .failed_approaches
        .iter()
        .map(|a| (a.id.clone(), a.action.clone()))
        .collect();
    let verified: Vec<_> = rec
        .verified_repairs
        .iter()
        .map(|r| (r.id.clone(), r.verified_by.clone().unwrap_or_default()))
        .collect();
    let open: Vec<_> = rec.open_hypotheses.iter().map(|h| h.id.clone()).collect();
    let esc: Vec<_> = rec
        .escalations
        .iter()
        .map(|e| (e.id.clone(), e.reason.clone(), e.outstanding.clone()))
        .collect();
    let status = if rec.session.status == chip_session_memory::SessionStatus::Escalated {
        "escalated"
    } else {
        "active"
    };
    let plan = rec.plan.as_ref().map(|p| p.steps.clone());
    let pending = rec.session.pending.as_ref().map(|p| p.description.clone());
    let checkpoint = rec.checkpoint.as_ref().map(|c| c.id.clone());
    // Plan revisions are counted by the store; the expectation counts plan events.
    packet(
        &w.objective(),
        status,
        plan.as_ref(),
        rec.plan.as_ref().map_or(0, |p| p.revision),
        &failed,
        &verified,
        &unresolved,
        &open,
        &esc,
        checkpoint.as_ref(),
        pending.as_ref(),
        rec.observations,
    )
}

fn arm_felt(w: &Workload, dir: &Path, keep_payloads: bool) -> Value {
    let name = if keep_payloads {
        "felt_store_all"
    } else {
        "felt_summaries_only"
    };
    let mut out = json!({"arm": name});
    out["threads_start"] = json!(threads());
    out["memory_start"] = mem();
    let _ = std::fs::remove_dir_all(dir);
    let root = dir.to_path_buf();
    let mut m = SessionMemory::create(&root, "bench", &w.objective()).expect("create");
    let mut facts = Facts::default();
    let mut write = Latencies::new();
    let started = Instant::now();
    for e in w.events() {
        let op = Instant::now();
        facts.note(&e);
        // felt_summaries_only keeps only test diagnostics (what recovery may need) and drops the
        // payloads of reads, lists, searches and the like at ingest.
        let keep = keep_payloads || matches!(&e, Event::Observation { kind, .. } if kind == "test");
        m.apply(e, keep).expect("apply");
        write.record(op);
    }
    out["timing_ms"] = json!({"ingest_total": ms(started)});
    out["latency_us"] = json!({"write": write.summary()});
    out["memory_after_ingest"] = mem();
    let before = m.stats().expect("stats");

    // Reads and queries.
    let all = m.observations().expect("observations");
    let mut read = Latencies::new();
    for i in 0..2000usize {
        let o = &all[(i * 7919) % all.len()];
        let op = Instant::now();
        let _ = m.observation(&o.id).unwrap();
        let _ = m.payload(&o.id).unwrap();
        read.record(op);
    }
    out["latency_us"]["read"] = read.summary();
    let t = Instant::now();
    let _ = m.test_results().unwrap();
    let filtered = ms(t);
    let t = Instant::now();
    let _ = m.observations().unwrap();
    out["timing_ms"]["full_scan_query"] = json!(ms(t));
    out["timing_ms"]["filtered_query_test_results"] = json!(filtered);
    // Serialization and deserialization of the records, in isolation.
    let t = Instant::now();
    let blobs: Vec<Vec<u8>> = all.iter().map(|o| serde_json::to_vec(o).unwrap()).collect();
    let ser = ms(t);
    let t = Instant::now();
    let back: Vec<chip_session_memory::ObservationRecord> = blobs
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    out["serialization_ms"] = json!({"records": all.len(), "serialize": ser, "deserialize": ms(t), "bytes": blobs.iter().map(Vec::len).sum::<usize>()});
    drop((blobs, back, all));

    let t = Instant::now();
    m.checkpoint("c-final").expect("checkpoint");
    out["timing_ms"]["checkpoint"] = json!(ms(t));

    let hwm_before = status_kib("VmHWM:");
    out["memory_before_compaction"] = mem();
    let t = Instant::now();
    let report = m.compact().expect("compact");
    out["timing_ms"]["compaction"] = json!(ms(t));
    out["memory_after_compaction"] = mem();
    out["memory_peak_during_compaction"] = json!({"hwm_before_kib": hwm_before, "hwm_after_kib": status_kib("VmHWM:"), "exceeded": status_kib("VmHWM:") > hwm_before});
    trim();
    out["memory_after_compaction_and_trim"] = mem();
    let after = m.stats().expect("stats");
    out["logical"] = json!({
        "records_before": before.records.values().sum::<u64>(), "records_after": after.records.values().sum::<u64>(),
        "payload_bytes_before": before.payload_bytes, "payload_bytes_after": after.payload_bytes,
        "record_bytes_before": before.record_bytes, "record_bytes_after": after.record_bytes,
        "purge_candidate_records": report.payloads_purged, "purge_candidate_bytes": report.payload_bytes_purged,
        "preserved_records": report.preserved_records, "preserved_bytes": report.preserved_bytes,
        "pinned_payload_records": report.payloads_pinned, "pinned_payload_bytes": report.payload_bytes_pinned,
    });
    out["store"] = json!({
        "live_rows_before": before.live_rows, "live_rows_after": after.live_rows,
        "journal_bytes_before": before.journal_bytes, "journal_bytes_after": after.journal_bytes,
        "revisions_collected": report.reclaim.revisions_collected, "operations_pruned": report.reclaim.operations_pruned,
        "reclaim_journal_before": report.reclaim.journal_before, "reclaim_journal_after": report.reclaim.journal_after,
    });

    // Destroy the instance, then rebuild the session from the persisted journal.
    m.close();
    drop(m);
    out["memory_after_close"] = mem();
    trim();
    out["memory_after_close_and_trim"] = mem();
    let t = Instant::now();
    let (m2, rec) = SessionMemory::recover(&root, "bench").expect("recover");
    let recovery_ms = ms(t);
    out["memory_after_recovery"] = mem();
    let pk = felt_packet(w, &m2, &rec);
    let pk_text = pk.to_string();
    let expected = expected_packet(w, &facts, |_| String::new());
    // The expectation computes the pinned payload digest from the same payload the adapter holds.
    let _ = expected;
    out["recovery"] = json!({"supported": true, "from": "disk", "elapsed_ms": recovery_ms, "packet_sha256": sha(&pk_text), "packet_bytes": pk_text.len(), "packet": pk, "payloads_held_after_recovery": rec.payloads_held, "pending_compaction": rec.pending_compaction});
    out["recovery_packet_expected_sha256_inputs"] = json!({"facts_unresolved": facts.unresolved().len(), "facts_open_hypotheses": facts.hypotheses.iter().filter(|h| h.1).count()});
    m2.destroy().expect("destroy");
    out["memory_after_destroy"] = mem();
    trim();
    out["memory_after_destroy_and_trim"] = mem();
    out["threads_end"] = json!(threads());
    out
}

// ------------------------------------------------------------------------------ driver

fn child(arm: &str, n: usize, seed: u64, dir: &Path) {
    let w = Workload::new(n, seed);
    let result = match arm {
        "chip_baseline" => arm_chip_baseline(&w),
        "memory_compact" => arm_memory_compact(&w),
        "felt_store_all" => arm_felt(&w, dir, true),
        "felt_summaries_only" => arm_felt(&w, dir, false),
        other => panic!("unknown arm {other}"),
    };
    let mut result = result;
    if std::env::var_os("SESSION_MEMORY_BENCH_DEBUG").is_some() {
        result["debug_packet"] = result["recovery"]["packet"].clone();
    }
    println!("RESULT {result}");
}

fn flatten(prefix: &str, v: &Value, out: &mut BTreeMap<String, f64>) {
    match v {
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                out.insert(prefix.to_string(), f);
            }
        }
        Value::Bool(b) => {
            out.insert(prefix.to_string(), if *b { 1.0 } else { 0.0 });
        }
        Value::Object(map) => {
            for (k, v) in map {
                flatten(
                    &if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    },
                    v,
                    out,
                );
            }
        }
        _ => {}
    }
}

fn stats(values: &[f64]) -> Value {
    let n = values.len() as f64;
    let mut s = values.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = s.iter().sum::<f64>() / n;
    let var = s.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    json!({"n": s.len(), "median": s[s.len() / 2], "min": s[0], "max": s[s.len() - 1], "mean": mean, "stddev": var.sqrt()})
}

fn sh(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn environment() -> Value {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("model name"))
        .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
        .unwrap_or_default();
    let mem_total = std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:").map(|v| v.trim().to_string()))
        .unwrap_or_default();
    json!({
        "kernel": std::fs::read_to_string("/proc/version").unwrap_or_default().trim(),
        "cpu_model": cpu, "logical_cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "mem_total": mem_total, "rustc": sh("rustc", &["-vV"]).lines().next().unwrap_or_default().to_string(),
        "glibc": sh("ldd", &["--version"]).lines().next().unwrap_or_default().to_string(),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release (cargo bench)" },
        "feltdb_rev": FELTDB_REV, "filesystem_for_journal": sh("df", &["-T", "--output=fstype", &std::env::temp_dir().to_string_lossy()]).lines().last().unwrap_or_default().to_string(),
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let quick = args.iter().any(|a| a == "--quick");
    let n: usize = get("--observations")
        .and_then(|v| v.parse().ok())
        .unwrap_or(if quick { 2000 } else { 10_000 });
    let seed: u64 = get("--seed")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20260401);
    if let Some(arm) = get("--child") {
        let dir = PathBuf::from(get("--dir").expect("--dir"));
        child(&arm, n, seed, &dir);
        return;
    }
    let trials: usize = get("--trials")
        .and_then(|v| v.parse().ok())
        .unwrap_or(if quick { 3 } else { 5 });
    let out_path = PathBuf::from(get("--out").unwrap_or_else(|| {
        std::env::var("SESSION_MEMORY_BENCH_JSON").unwrap_or_else(|_| {
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../target/session-memory-bench.json"
            )
            .into()
        })
    }));
    let exe = std::env::current_exe().unwrap();
    let work =
        std::env::temp_dir().join(format!("chip-session-memory-bench-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();

    let mut raw: BTreeMap<&str, Vec<Value>> = ARMS.iter().map(|a| (*a, Vec::new())).collect();
    for trial in 0..trials {
        for arm in ARMS {
            let dir = work.join(format!("{arm}-{trial}"));
            let started = Instant::now();
            let output = Command::new(&exe)
                .args([
                    "--child",
                    arm,
                    "--observations",
                    &n.to_string(),
                    "--seed",
                    &seed.to_string(),
                    "--dir",
                    &dir.to_string_lossy(),
                ])
                .output()
                .expect("child runs");
            if !output.status.success() {
                panic!(
                    "{arm} trial {trial} failed:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let text = String::from_utf8_lossy(&output.stdout);
            let line = text
                .lines()
                .find_map(|l| l.strip_prefix("RESULT "))
                .expect("a RESULT line");
            let mut v: Value = serde_json::from_str(line).unwrap();
            v["wall_clock_total_ms"] = json!(ms(started));
            eprintln!(
                "  {arm:<20} trial {trial}: {:.1}s",
                started.elapsed().as_secs_f64()
            );
            raw.get_mut(arm).unwrap().push(v);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    let _ = std::fs::remove_dir_all(&work);

    // Summaries and the cross-arm recovery check.
    let mut arms = serde_json::Map::new();
    for (arm, trials_v) in &raw {
        let mut series: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for t in trials_v {
            let mut flat = BTreeMap::new();
            flatten("", t, &mut flat);
            for (k, v) in flat {
                series.entry(k).or_default().push(v);
            }
        }
        let summary: serde_json::Map<String, Value> =
            series.iter().map(|(k, v)| (k.clone(), stats(v))).collect();
        arms.insert(
            (*arm).into(),
            json!({"trials": trials_v, "summary": summary}),
        );
    }
    let digests: Vec<Vec<String>> = ARMS
        .iter()
        .map(|a| {
            raw[a]
                .iter()
                .map(|t| {
                    t["recovery"]["packet_sha256"]
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                })
                .collect()
        })
        .collect();
    let equivalent = (0..trials).all(|t| digests.iter().all(|d| d[t] == digests[0][t]));
    let doc = json!({
        "schema": "chip.session-memory-bench.v1",
        "units": {"rss_kib": "KiB", "hwm_kib": "KiB (peak RSS, monotonic)", "*_bytes": "bytes", "*_ms": "milliseconds", "*_us": "microseconds", "counts": "records"},
        "configuration": {"observations": n, "trials": trials, "seed": seed, "arms": ARMS, "payload_size_distribution": "60% 1 KiB, 30% 8 KiB, 9% 32 KiB, 1% 128 KiB", "duplicate_rate": "about 30% repeat an earlier payload exactly", "feltdb_durability": "FeltDB default (Flushed): written to the OS before each call returns, fsync only at checkpoint"},
        "method": "Each arm and trial in a fresh child process, trials interleaved across arms. Memory points are VmRSS/VmHWM from /proc/self/status and glibc mallinfo2 for the main arena (the workload runs on the main thread). 'and_trim' points follow an explicit malloc_trim(0). The recovery packet is a canonical JSON of the facts a recovery or escalation needs, hashed; the check is that every arm produced the same packet in every trial. Latencies are per workload event (write) and per operation (read).",
        "environment": environment(),
        "recovery_packets_equivalent_across_arms": equivalent,
        "arms": arms,
    });
    if let Some(dir) = out_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&out_path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

    // A readable summary of the medians.
    let med = |arm: &str, key: &str| doc["arms"][arm]["summary"][key]["median"].as_f64();
    let rows: [(&str, &str, f64); 14] = [
        ("RSS at start (MiB)", "memory_start.rss_kib", 1024.0),
        (
            "RSS after ingest (MiB)",
            "memory_after_ingest.rss_kib",
            1024.0,
        ),
        (
            "peak RSS after ingest (MiB)",
            "memory_after_ingest.hwm_kib",
            1024.0,
        ),
        (
            "heap in use after ingest (MiB)",
            "memory_after_ingest.heap_in_use_bytes",
            1048576.0,
        ),
        (
            "RSS after compaction (MiB)",
            "memory_after_compaction.rss_kib",
            1024.0,
        ),
        (
            "heap in use after compaction (MiB)",
            "memory_after_compaction.heap_in_use_bytes",
            1048576.0,
        ),
        (
            "RSS after compaction + trim (MiB)",
            "memory_after_compaction_and_trim.rss_kib",
            1024.0,
        ),
        (
            "RSS after destroy (MiB)",
            "memory_after_destroy.rss_kib",
            1024.0,
        ),
        (
            "RSS after destroy + trim (MiB)",
            "memory_after_destroy_and_trim.rss_kib",
            1024.0,
        ),
        ("ingest (ms)", "timing_ms.ingest_total", 1.0),
        ("compaction (ms)", "timing_ms.compaction", 1.0),
        ("write p50 (us)", "latency_us.write.p50_us", 1.0),
        ("write p99 (us)", "latency_us.write.p99_us", 1.0),
        ("read p50 (us)", "latency_us.read.p50_us", 1.0),
    ];
    println!("\nsession-memory benchmark: {n} observations, {trials} trials (medians)\n");
    print!("{:<38}", "");
    for a in ARMS {
        print!("{a:>22}");
    }
    println!();
    for (label, key, div) in rows {
        print!("{label:<38}");
        for a in ARMS {
            match med(a, key) {
                Some(v) => print!("{:>22.1}", v / div),
                None => print!("{:>22}", "n/a"),
            }
        }
        println!();
    }
    println!("\nrecovery packets identical across arms and trials: {equivalent}");
    println!("wrote {}", out_path.display());
}
