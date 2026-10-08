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
use chip_session_memory::backend::{self, Backend, Felt, Redb, Sqlite};
use chip_session_memory::packet::packet_and_hash;
use chip_session_memory::workload::{Event, Workload};
use chip_session_memory::{Outcome, SessionMemory};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Every arm. The first four are the original session-memory experiment; the rest compare durable
/// stores under identical retention (`*_store_all`) and under fsync-every-write (`*_synced`).
const ALL_ARMS: [&str; 11] = [
    "chip_baseline",
    "memory_compact",
    "felt_store_all",
    "felt_summaries_only",
    "sqlite_store_all",
    "redb_store_all",
    "redb_default_cache",
    "felt_synced",
    "sqlite_synced",
    "redb_synced",
    "redb_default_cache_synced",
];
/// Arms run when `--arms` is not given.
const DEFAULT_ARMS: [&str; 6] = [
    "chip_baseline",
    "memory_compact",
    "felt_store_all",
    "sqlite_store_all",
    "redb_store_all",
    "redb_default_cache",
];
const DURABLE_ARMS: [&str; 9] = [
    "felt_store_all",
    "felt_summaries_only",
    "sqlite_store_all",
    "redb_store_all",
    "redb_default_cache",
    "felt_synced",
    "sqlite_synced",
    "redb_synced",
    "redb_default_cache_synced",
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
        "rss_anon_kib": status_kib("RssAnon:"),
        "rss_file_kib": status_kib("RssFile:"),
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
        json!({"count": n, "p50_us": at(0.5), "p95_us": at(0.95), "p99_us": at(0.99), "mean_us": self.0.iter().sum::<f64>() / n as f64, "max_us": self.0[n - 1]})
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

/// `Cached` from /proc/meminfo, in KiB: the system-wide page cache. Indicative only (other
/// processes move it), reported as a delta around the arm.
fn page_cache_kib() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Cached:").map(str::to_string))
        })
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

/// Applies the per-arm engine configuration. Returns (keep_payloads).
fn configure(arm: &str) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    backend::SYNC_EVERY_WRITE.store(arm.ends_with("_synced"), Relaxed);
    if arm.starts_with("redb_default_cache") {
        backend::redb_store::CACHE_BYTES_OVERRIDE
            .store(backend::redb_store::REDB_DEFAULT_CACHE_BYTES, Relaxed);
    }
    arm != "felt_summaries_only"
}

fn arm_store<B: Backend>(arm: &str, w: &Workload, dir: &Path) -> Value {
    let keep_payloads = configure(arm);
    let mut out = json!({"arm": arm, "engine": B::NAME});
    out["threads_start"] = json!(threads());
    out["memory_start"] = mem();
    let cache_start = page_cache_kib();
    let _ = std::fs::remove_dir_all(dir);
    let root = dir.to_path_buf();
    let mut m = SessionMemory::<B>::create_with(&root, "bench", &w.objective()).expect("create");
    let mut facts = Facts::default();
    let mut write = Latencies::new();
    let mut ckpt = Latencies::new();
    let started = Instant::now();
    for e in w.events() {
        let op = Instant::now();
        facts.note(&e);
        // felt_summaries_only keeps only test diagnostics (what recovery may need) and drops the
        // payloads of reads, lists, searches and the like at ingest.
        let keep = keep_payloads || matches!(&e, Event::Observation { kind, .. } if kind == "test");
        let is_ckpt = matches!(e, Event::Checkpoint { .. });
        m.apply(e, keep).expect("apply");
        if is_ckpt {
            ckpt.record(op);
        } else {
            write.record(op);
        }
    }
    out["timing_ms"] = json!({"ingest_total": ms(started)});
    out["latency_us"] = json!({"write": write.summary(), "checkpoint": ckpt.summary()});
    out["memory_after_ingest"] = mem();
    out["page_cache_delta_kib_after_ingest"] = json!(page_cache_kib() as i64 - cache_start as i64);
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
    drop(all);

    let t = Instant::now();
    m.checkpoint("c-final").expect("checkpoint");
    out["timing_ms"]["checkpoint_final"] = json!(ms(t));

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
        "disk_bytes_before_compaction": before.journal_bytes, "disk_bytes_after_compaction": after.journal_bytes,
        "revisions_collected": report.reclaim.revisions_collected, "operations_pruned": report.reclaim.operations_pruned,
        "reclaim_disk_before": report.reclaim.journal_before, "reclaim_disk_after": report.reclaim.journal_after,
        "disk_amplification_before": before.journal_bytes as f64 / (before.payload_bytes + before.record_bytes).max(1) as f64,
    });

    // Destroy the instance, then rebuild the session from what is on disk.
    m.close();
    drop(m);
    out["memory_after_close"] = mem();
    trim();
    out["memory_after_close_and_trim"] = mem();
    let t = Instant::now();
    let (mut m2, rec) = SessionMemory::<B>::recover_with(&root, "bench").expect("recover");
    let recovery_ms = ms(t);
    out["memory_after_recovery"] = mem();
    let (pk_text, pk_sha) = packet_and_hash(&m2, &rec);
    let now = m2.stats().expect("stats");
    let (mut lost, mut duplicated) = (0u64, 0u64);
    for c in chip_session_memory::coll::ALL {
        let (a, b) = (
            after.records.get(c).copied().unwrap_or(0),
            now.records.get(c).copied().unwrap_or(0),
        );
        lost += a.saturating_sub(b);
        duplicated += b.saturating_sub(a);
    }
    let integrity = m2.integrity().unwrap_or_else(|e| e.to_string());
    out["recovery"] = json!({"supported": true, "from": "disk", "elapsed_ms": recovery_ms, "packet_sha256": pk_sha, "packet_bytes": pk_text.len(), "payloads_held_after_recovery": rec.payloads_held, "pending_compaction": rec.pending_compaction, "records_lost": lost, "records_duplicated": duplicated, "engine_integrity": integrity});
    m2.destroy().expect("destroy");
    out["memory_after_destroy"] = mem();
    trim();
    out["memory_after_destroy_and_trim"] = mem();
    out["threads_end"] = json!(threads());
    out
}

/// Kill-recovery phase 1: ingest everything, checkpoint, then die without any cleanup.
fn kill_phase1<B: Backend>(arm: &str, w: &Workload, dir: &Path) {
    configure(arm);
    let _ = std::fs::remove_dir_all(dir);
    let mut m = SessionMemory::<B>::create_with(dir, "bench", &w.objective()).expect("create");
    for e in w.events() {
        m.apply(e, true).expect("apply");
    }
    m.checkpoint("c-final").expect("checkpoint");
    // SAFETY: SIGKILL to ourselves: no destructor, flush or close runs.
    unsafe { kill(getpid(), 9) };
}

/// Kill-recovery phase 2, in a fresh process: open what the dead process left and measure.
fn kill_phase2<B: Backend>(arm: &str, dir: &Path) -> Value {
    configure(arm);
    let mut out = json!({"arm": arm, "memory_start": mem()});
    let t = Instant::now();
    let (mut m, rec) = SessionMemory::<B>::recover_with(dir, "bench").expect("recover after kill");
    out["elapsed_ms"] = json!(ms(t));
    out["memory_after_recovery"] = mem();
    let (_, sha) = packet_and_hash(&m, &rec);
    let t = Instant::now();
    let integrity = m.integrity().unwrap_or_else(|e| e.to_string());
    out["integrity_check_ms"] = json!(ms(t));
    out["packet_sha256"] = json!(sha);
    out["engine_integrity"] = json!(integrity);
    out["observations"] = json!(rec.observations);
    out["checkpoint"] = json!(rec.checkpoint.map(|c| c.id));
    out["disk_bytes"] = json!(m.stats().unwrap().journal_bytes);
    out
}

fn dispatch<T>(
    arm: &str,
    felt: impl FnOnce() -> T,
    sqlite: impl FnOnce() -> T,
    redb: impl FnOnce() -> T,
) -> T {
    if arm.starts_with("felt") {
        felt()
    } else if arm.starts_with("sqlite") {
        sqlite()
    } else if arm.starts_with("redb") {
        redb()
    } else {
        panic!("unknown durable arm {arm}")
    }
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn getpid() -> i32;
}

// ------------------------------------------------------------------------------ driver

fn child(arm: &str, n: usize, seed: u64, dir: &Path) {
    let w = Workload::new(n, seed);
    let result = match arm {
        "chip_baseline" => arm_chip_baseline(&w),
        "memory_compact" => arm_memory_compact(&w),
        durable if DURABLE_ARMS.contains(&durable) => dispatch(
            durable,
            || arm_store::<Felt>(durable, &w, dir),
            || arm_store::<Sqlite>(durable, &w, dir),
            || arm_store::<Redb>(durable, &w, dir),
        ),
        other => panic!("unknown arm {other}"),
    };
    println!("RESULT {result}");
}

fn child_kill1(arm: &str, n: usize, seed: u64, dir: &Path) {
    let w = Workload::new(n, seed);
    dispatch(
        arm,
        || kill_phase1::<Felt>(arm, &w, dir),
        || kill_phase1::<Sqlite>(arm, &w, dir),
        || kill_phase1::<Redb>(arm, &w, dir),
    );
}

fn child_kill2(arm: &str, dir: &Path) {
    let result = dispatch(
        arm,
        || kill_phase2::<Felt>(arm, dir),
        || kill_phase2::<Sqlite>(arm, dir),
        || kill_phase2::<Redb>(arm, dir),
    );
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
        "feltdb_rev": FELTDB_REV, "sqlite": "rusqlite 0.40.2 with bundled SQLite 3.53.2", "redb": "4.3.0", "build": "cargo bench (release profile)", "filesystem_for_journal": sh("df", &["-T", "--output=fstype", &std::env::temp_dir().to_string_lossy()]).lines().last().unwrap_or_default().to_string(),
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
    // Profiles. Stress is the original 10,000-observation workload. Normal is sized to Chip's
    // bounded work runs: the CLI and service defaults are 12 turns and 8 executions, the ceiling
    // is 50 of each, so one run at the ceiling observes on the order of 100 results (a turn's
    // observation plus each execution's), drawn from the same payload mix.
    let profile = get("--profile");
    let profile_n = match profile.as_deref() {
        Some("normal") => Some(100),
        Some("stress") => Some(10_000),
        Some(other) => panic!("unknown profile {other}: normal or stress"),
        None => None,
    };
    let n: usize = get("--observations")
        .and_then(|v| v.parse().ok())
        .or(profile_n)
        .unwrap_or(if quick { 2000 } else { 10_000 });
    let seed: u64 = get("--seed")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20260401);
    if let Some(arm) = get("--kill1") {
        child_kill1(&arm, n, seed, &PathBuf::from(get("--dir").expect("--dir")));
        return;
    }
    if let Some(arm) = get("--kill2") {
        child_kill2(&arm, &PathBuf::from(get("--dir").expect("--dir")));
        return;
    }
    if let Some(arm) = get("--child") {
        let dir = PathBuf::from(get("--dir").expect("--dir"));
        child(&arm, n, seed, &dir);
        return;
    }
    let trials: usize = get("--trials")
        .and_then(|v| v.parse().ok())
        .unwrap_or(if quick { 3 } else { 5 });
    let arms_arg = get("--arms");
    let arms: Vec<&'static str> = match &arms_arg {
        Some(list) => list
            .split(',')
            .map(|a| {
                *ALL_ARMS
                    .iter()
                    .find(|k| **k == a)
                    .unwrap_or_else(|| panic!("unknown arm {a}"))
            })
            .collect(),
        None => DEFAULT_ARMS.to_vec(),
    };
    let kill_recovery = args.iter().any(|a| a == "--kill-recovery");
    let default_json = match &profile {
        Some(p) => format!(
            "{}/../../target/session-store-bench-{p}.json",
            env!("CARGO_MANIFEST_DIR")
        ),
        None => concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/session-memory-bench.json"
        )
        .into(),
    };
    let out_path = PathBuf::from(
        get("--out")
            .unwrap_or_else(|| std::env::var("SESSION_MEMORY_BENCH_JSON").unwrap_or(default_json)),
    );
    let exe = std::env::current_exe().unwrap();
    let work =
        std::env::temp_dir().join(format!("chip-session-memory-bench-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();

    let mut raw: BTreeMap<&str, Vec<Value>> = arms.iter().map(|a| (*a, Vec::new())).collect();
    for trial in 0..trials {
        for &arm in &arms {
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
            let _ = std::fs::remove_dir_all(&dir);
            if kill_recovery && DURABLE_ARMS.contains(&arm) {
                // A fresh process ingests and is SIGKILLed; another fresh process recovers.
                let kdir = work.join(format!("{arm}-kill-{trial}"));
                let first = Command::new(&exe)
                    .args(["--kill1", arm, "--observations", &n.to_string(), "--seed"])
                    .args([&seed.to_string(), "--dir", &kdir.to_string_lossy()])
                    .output()
                    .expect("kill child runs");
                assert!(
                    !first.status.success(),
                    "{arm}: the first process was meant to die"
                );
                let second = Command::new(&exe)
                    .args(["--kill2", arm, "--dir", &kdir.to_string_lossy()])
                    .output()
                    .expect("recovery child runs");
                if !second.status.success() {
                    panic!(
                        "{arm} trial {trial}: recovery after SIGKILL failed:\n{}",
                        String::from_utf8_lossy(&second.stderr)
                    );
                }
                let text = String::from_utf8_lossy(&second.stdout);
                let line = text
                    .lines()
                    .find_map(|l| l.strip_prefix("RESULT "))
                    .expect("RESULT");
                v["recovery_after_kill"] = serde_json::from_str(line).unwrap();
                let _ = std::fs::remove_dir_all(&kdir);
            }
            raw.get_mut(arm).unwrap().push(v);
        }
    }
    let _ = std::fs::remove_dir_all(&work);

    // Summaries and the cross-arm recovery check.
    let mut arm_docs = serde_json::Map::new();
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
        arm_docs.insert(
            (*arm).into(),
            json!({"trials": trials_v, "summary": summary}),
        );
    }
    let digests: Vec<Vec<String>> = arms
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
    let kill_equivalent = (0..trials).all(|t| {
        arms.iter().all(|a| {
            raw[a][t]["recovery_after_kill"]["packet_sha256"].is_null()
                || raw[a][t]["recovery_after_kill"]["packet_sha256"]
                    == raw[a][t]["recovery"]["packet_sha256"]
        })
    });
    let doc = json!({
        "schema": "chip.session-memory-bench.v2", "profile": profile,
        "units": {"rss_kib": "KiB", "hwm_kib": "KiB (peak RSS, monotonic)", "*_bytes": "bytes", "*_ms": "milliseconds", "*_us": "microseconds", "counts": "records"},
        "configuration": {"observations": n, "trials": trials, "seed": seed, "arms": arms, "kill_recovery": kill_recovery, "payload_size_distribution": "60% 1 KiB, 30% 8 KiB, 9% 32 KiB, 1% 128 KiB", "duplicate_rate": "about 30% repeat an earlier payload exactly", "feltdb_durability": "FeltDB default (Flushed): written to the OS before each call returns, fsync only at checkpoint"},
        "method": "Each arm and trial in a fresh child process, trials interleaved across arms. Memory points are VmRSS/VmHWM from /proc/self/status and glibc mallinfo2 for the main arena (the workload runs on the main thread). 'and_trim' points follow an explicit malloc_trim(0). The recovery packet is a canonical JSON of the facts a recovery or escalation needs, hashed; the check is that every arm produced the same packet in every trial. Latencies are per workload event (write) and per operation (read).",
        "environment": environment(),
        "recovery_packets_equivalent_across_arms": equivalent,
        "recovery_after_kill_matches_clean_recovery": kill_equivalent,
        "arms": arm_docs,
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
    for a in &arms {
        print!("{a:>22}");
    }
    println!();
    for (label, key, div) in rows {
        print!("{label:<38}");
        for a in &arms {
            match med(a, key) {
                Some(v) => print!("{:>22.1}", v / div),
                None => print!("{:>22}", "n/a"),
            }
        }
        println!();
    }
    println!("\nrecovery packets identical across arms and trials: {equivalent}");
    println!("recovery after SIGKILL matches clean recovery: {kill_equivalent}");
    println!("wrote {}", out_path.display());
}
