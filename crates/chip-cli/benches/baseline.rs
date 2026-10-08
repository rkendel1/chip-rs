//! Rust Chip performance baseline. A measurement, not an optimization: nothing here changes the
//! product, and no number here is a budget.
//!
//! ```text
//! cargo bench -p chip-cli --bench baseline                  # everything
//! cargo bench -p chip-cli --bench baseline -- --quick       # fewer samples (CI smoke)
//! cargo bench -p chip-cli --bench baseline -- --only loop   # one section (see SECTIONS)
//! ```
//!
//! Three tiers, kept apart so that Chip's own overhead is never confused with execution or with
//! a model:
//!
//! * **L0 runtime overhead**: a synthetic in-process model and an in-process capability. This is
//!   Chip's machinery and nothing else.
//! * **L1 real capability**: real filesystem, Git and (when installed) PAX, still a synthetic model.
//! * **L2 real model**: only with `CHIP_BENCH_REAL_MODEL=1` and `CHIP_PROVIDER`/`CHIP_MODEL`/
//!   `CHIP_ENDPOINT`; otherwise it says SKIPPED. A skipped tier is reported as missing, never filled.
//!
//! Chip overhead of a run is `total - model - execution`, all three measured by the work loop itself
//! (`WorkLatency`). The synthetic model answers instantly, so its time is part of "model" and is
//! reported, not hidden.
//!
//! Counts are asserted (one model call per escalation, one execution per executed request, no
//! retries): a baseline of a loop that makes extra calls would measure the wrong thing. Times are
//! only reported.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chip_cli::local_environment::{LocalEnvironment, LocalEnvironmentProvider, opaque_id};
use chip_cli::provider_selection::Selection;
use chip_cli::service::{Capacity, Service};
use chip_cli::software_work::{CompleteWhenVerified, WorkRuntime, run_software_work_with_budget};
use chip_core::{
    Agent, Capability, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, CapabilitySet, EnvironmentDescription, EnvironmentError, EnvironmentId,
    EnvironmentProvider, Environments, ExecutionError, ExecutionId, ExecutionObserver,
    ExecutionRequest, ExecutionResult, ExecutionStatus, Executor, InputValue,
    ModelDecisionBoundary, NoLocalPolicy, Observation, ObservationKind, ObservationPredicate,
    Observer, WorkDecisionBoundary, WorkEnvironment, WorkEvent, WorkGoal, WorkId, WorkLimits,
    WorkSpec,
};
use chip_pax::PaxExecutor;
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SECTIONS: &[&str] = &[
    "parts",     // L0: parse, validate, dispatch, environment, observe/evidence, events, goal
    "loop",      // L0: the whole loop, synthetic model, in-process capability
    "real",      // L1: the whole loop over real filesystem / Git / PAX
    "process",   // binary startup, `chip work` start overhead, `chip serve`
    "serve",     // concurrent throughput through the HTTP service
    "realmodel", // L2
];

// ---- reporting -------------------------------------------------------------------------------------

#[derive(Clone)]
struct Row {
    tier: &'static str,
    name: String,
    n: usize,
    unit: &'static str,
    min: f64,
    p50: f64,
    p95: f64,
    max: f64,
    note: String,
}

#[derive(Default)]
struct Report {
    rows: Vec<Row>,
    skipped: Vec<(String, String)>,
    facts: Vec<(String, String)>,
}

impl Report {
    /// Records a series of durations, in microseconds.
    fn time(
        &mut self,
        tier: &'static str,
        name: impl Into<String>,
        mut d: Vec<Duration>,
        note: &str,
    ) {
        d.sort();
        let us = |x: Duration| x.as_secs_f64() * 1e6;
        let pick = |q: f64| us(d[((d.len() - 1) as f64 * q).round() as usize]);
        self.rows.push(Row {
            tier,
            name: name.into(),
            n: d.len(),
            unit: "us",
            min: us(d[0]),
            p50: pick(0.5),
            p95: pick(0.95),
            max: us(*d.last().unwrap()),
            note: note.into(),
        });
    }

    /// Records a scalar (one sample).
    fn value(
        &mut self,
        tier: &'static str,
        name: impl Into<String>,
        unit: &'static str,
        v: f64,
        note: &str,
    ) {
        self.rows.push(Row {
            tier,
            name: name.into(),
            n: 1,
            unit,
            min: v,
            p50: v,
            p95: v,
            max: v,
            note: note.into(),
        });
    }

    fn skip(&mut self, what: &str, why: &str) {
        eprintln!("SKIPPED {what}: {why}");
        self.skipped.push((what.into(), why.into()));
    }

    fn fact(&mut self, k: &str, v: impl Into<String>) {
        self.facts.push((k.into(), v.into()));
    }

    fn print(&self) {
        println!("\n== Rust Chip performance baseline ==");
        for (k, v) in &self.facts {
            println!("{k}: {v}");
        }
        for tier in ["L0", "L1", "L2", "process", "service"] {
            let rows: Vec<&Row> = self.rows.iter().filter(|r| r.tier == tier).collect();
            if rows.is_empty() {
                continue;
            }
            println!("\n-- {tier} --");
            println!(
                "{:<58} {:>5} {:>11} {:>11} {:>11} {:>11} {}",
                "metric", "n", "min", "p50", "p95", "max", "unit"
            );
            for r in rows {
                println!(
                    "{:<58} {:>5} {:>11.1} {:>11.1} {:>11.1} {:>11.1} {}{}",
                    r.name,
                    r.n,
                    r.min,
                    r.p50,
                    r.p95,
                    r.max,
                    r.unit,
                    if r.note.is_empty() {
                        String::new()
                    } else {
                        format!("  ({})", r.note)
                    }
                );
            }
        }
        if !self.skipped.is_empty() {
            println!("\n-- skipped (no number is claimed for these) --");
            for (what, why) in &self.skipped {
                println!("{what}: {why}");
            }
        }
    }

    fn json(&self) -> String {
        let esc = |s: &str| serde_json::to_string(s).unwrap();
        let rows: Vec<String> = self
            .rows
            .iter()
            .map(|r| {
                format!(
                    "{{\"tier\":{},\"name\":{},\"n\":{},\"unit\":{},\"min\":{:.3},\"p50\":{:.3},\"p95\":{:.3},\"max\":{:.3},\"note\":{}}}",
                    esc(r.tier), esc(&r.name), r.n, esc(r.unit), r.min, r.p50, r.p95, r.max, esc(&r.note)
                )
            })
            .collect();
        let facts: Vec<String> = self
            .facts
            .iter()
            .map(|(k, v)| format!("{}:{}", esc(k), esc(v)))
            .collect();
        let skipped: Vec<String> = self
            .skipped
            .iter()
            .map(|(k, v)| format!("{}:{}", esc(k), esc(v)))
            .collect();
        format!(
            "{{\"schema\":\"chip.perf-baseline.v1\",\"facts\":{{{}}},\"skipped\":{{{}}},\"rows\":[{}]}}\n",
            facts.join(","),
            skipped.join(","),
            rows.join(",")
        )
    }
}

struct Opts {
    quick: bool,
    only: Option<String>,
}

impl Opts {
    fn samples(&self, full: usize) -> usize {
        if self.quick { (full / 10).max(3) } else { full }
    }
    fn wants(&self, section: &str) -> bool {
        self.only.as_deref().is_none_or(|o| o == section)
    }
}

// ---- small helpers ---------------------------------------------------------------------------------

/// Resident memory of a process in KiB: (current, peak). Linux only.
fn memory(pid: Option<u32>) -> Option<(u64, u64)> {
    let path = match pid {
        Some(p) => format!("/proc/{p}/status"),
        None => "/proc/self/status".into(),
    };
    let text = std::fs::read_to_string(path).ok()?;
    let field = |name: &str| {
        text.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
    };
    Some((field("VmRSS:")?, field("VmHWM:")?))
}

fn percentile(sorted: &[Duration], q: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-bench-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A small Rust project whose test fails until `canonical` exists.
fn fixture(tag: &str) -> PathBuf {
    let root = temp_dir(tag);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname = \"bench_{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"benchfx\"\n",
            tag.replace('-', "_")
        ),
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), FIXTURE_OLD).unwrap();
    std::fs::write(
        root.join("tests/canon.rs"),
        "use benchfx::canonical;\n\n#[test]\nfn sorted() {\n    assert_eq!(canonical(\"b=2&a=1\"), \"a=1&b=2\");\n}\n",
    )
    .unwrap();
    for i in 0..20 {
        std::fs::write(
            root.join("src").join(format!("m{i}.rs")),
            format!("// module {i}\n"),
        )
        .unwrap();
    }
    root
}

const FIXTURE_OLD: &str = "pub fn len(s: &str) -> usize {\n    s.len()\n}\n";
const FIXTURE_NEW: &str = "pub fn len(s: &str) -> usize {\n    s.len()\n}\n\npub fn canonical(s: &str) -> String {\n    let mut p: Vec<&str> = s.split('&').collect();\n    p.sort();\n    p.join(\"&\")\n}\n";

/// Whether a real PAX is installed (not just something that answers `--version`).
async fn real_pax(root: &Path) -> Option<String> {
    let pax = PaxExecutor::new(root);
    pax.resolve()
        .await
        .ok()
        .map(|r| format!("pax {}", r.version))
}

// ---- the synthetic model and the in-process capability ---------------------------------------------

/// Answers instantly with the next scripted reply. Counts its calls.
struct Synthetic {
    replies: Mutex<std::collections::VecDeque<String>>,
    calls: AtomicUsize,
}

impl Synthetic {
    fn new(replies: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl ModelProvider for Synthetic {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, FxError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let reply =
            self.replies.lock().unwrap().pop_front().ok_or_else(|| {
                FxError::Provider("the synthetic model ran out of replies".into())
            })?;
        Ok(ModelResponse::new("synthetic", reply, Usage::new(100, 20)))
    }
}

/// An in-process capability that does nothing: dispatch cost without execution cost.
struct Noop {
    executed: AtomicUsize,
}

const NOOP: &str = "bench.noop";

#[async_trait::async_trait]
impl CapabilityProvider for Noop {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(vec![
            CapabilityDescriptor::new(
                CapabilityId::new(NOOP)?,
                "Noop",
                "An in-process capability that does nothing",
            )
            .without_evidence_reuse(),
        ])
    }
    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

#[async_trait::async_trait]
impl Executor for Noop {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        let n = self.executed.fetch_add(1, Ordering::SeqCst);
        Ok(ExecutionResult {
            id: request.id,
            status: ExecutionStatus::Success,
            output: "ok".into(),
            receipt_id: Some(format!("sha256:{n}")),
        })
    }
}

fn request_json(capability: &str, inputs: &str) -> String {
    if inputs.is_empty() {
        format!("{{\"decision\":\"request_capability\",\"capability\":\"{capability}\"}}")
    } else {
        format!(
            "{{\"decision\":\"request_capability\",\"capability\":\"{capability}\",\"inputs\":{inputs}}}"
        )
    }
}

const COMPLETE: &str = "{\"decision\":\"complete\",\"summary\":\"done\"}";
const BLOCK: &str = "{\"decision\":\"block\",\"reason\":\"baseline\"}";

// ---- section: parts --------------------------------------------------------------------------------

async fn parts(report: &mut Report, opts: &Opts) {
    let n = opts.samples(2000);

    let root = fixture("parts");
    let pax = PaxExecutor::new(&root);
    let env = LocalEnvironment::new(
        opaque_id(&root),
        &root,
        pax,
        EnvironmentDescription::default(),
    );
    let set = env.capabilities();

    // -- decision parsing (model-independent) --
    let mut caps = Vec::new();
    for d in set.capabilities().await.unwrap() {
        let availability = set.availability(&d.id).await;
        caps.push(Capability {
            descriptor: d,
            availability,
        });
    }
    let boundary = ModelDecisionBoundary;
    let response = |text: &str| ModelResponse::new("r", text, Usage::new(1, 1));
    let cases: [(&str, ModelResponse); 4] = [
        (
            "request with inputs",
            response(&request_json("project.read", "{\"path\":\"src/lib.rs\"}")),
        ),
        ("complete", response(COMPLETE)),
        (
            "rejected: unknown capability",
            response(&request_json("no.such", "")),
        ),
        (
            "rejected: not JSON",
            response("I think we should read the file"),
        ),
    ];
    for (name, resp) in cases {
        let mut d = Vec::with_capacity(n);
        for _ in 0..n {
            let t = Instant::now();
            let _ = std::hint::black_box(boundary.interpret(std::hint::black_box(&resp), &caps));
            d.push(t.elapsed());
        }
        report.time(
            "L0",
            format!("decision parse: {name}"),
            d,
            "ModelDecisionBoundary::interpret",
        );
    }

    // -- capability validation --
    let read = CapabilityId::new("project.read").unwrap();
    let ok_inputs: BTreeMap<String, InputValue> =
        [("path".to_string(), InputValue::Text("src/lib.rs".into()))].into();
    let bad_inputs: BTreeMap<String, InputValue> = [(
        "path".to_string(),
        InputValue::Text("../../etc/passwd".into()),
    )]
    .into();
    for (name, inputs) in [
        ("valid path", &ok_inputs),
        ("rejected path (..)", &bad_inputs),
    ] {
        let mut d = Vec::with_capacity(n);
        for _ in 0..n {
            let t = Instant::now();
            let _ = std::hint::black_box(set.validate_inputs(&read, inputs).await);
            d.push(t.elapsed());
        }
        report.time(
            "L1",
            format!("capability validate: project.read, {name}"),
            d,
            "CapabilitySet over project+pax backends",
        );
    }
    let noop_set = CapabilitySet::new().with(Arc::new(Noop {
        executed: AtomicUsize::new(0),
    }));
    let noop_id = CapabilityId::new(NOOP).unwrap();
    let mut d = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        let _ = std::hint::black_box(noop_set.validate_inputs(&noop_id, &BTreeMap::new()).await);
        d.push(t.elapsed());
    }
    report.time("L0", "capability validate: in-process noop", d, "");
    let mut d = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        let _ = std::hint::black_box(set.availability(&read).await);
        d.push(t.elapsed());
    }
    report.time(
        "L1",
        "capability availability: project.read",
        d,
        "CapabilitySet routing + backend check",
    );

    // -- capability dispatch --
    let mut d = Vec::with_capacity(n);
    for i in 0..n {
        let request = ExecutionRequest::new(ExecutionId::new(format!("e{i}")), NOOP);
        let t = Instant::now();
        let _ = std::hint::black_box(noop_set.execute(request).await);
        d.push(t.elapsed());
    }
    report.time(
        "L0",
        "capability dispatch: noop through CapabilitySet",
        d,
        "routing + a do-nothing executor",
    );
    let mut d = Vec::with_capacity(n);
    for i in 0..n {
        let request = ExecutionRequest::new(ExecutionId::new(format!("l{i}")), "project.list")
            .with_inputs([("path".to_string(), InputValue::Text(".".into()))].into());
        let t = Instant::now();
        let _ = std::hint::black_box(set.execute(request).await);
        d.push(t.elapsed());
    }
    report.time(
        "L1",
        "capability execute: project.list (24 entries)",
        d,
        "real filesystem",
    );

    // -- environment acquisition / release --
    {
        let provider = Arc::new(LocalEnvironmentProvider::prepare(&root).await);
        match provider.as_ref() {
            Ok(_) => {
                let provider = Arc::new(LocalEnvironmentProvider::prepare(&root).await.unwrap());
                let envs = Environments::new(provider);
                let mut d = Vec::with_capacity(n.min(500));
                for i in 0..n.min(500) {
                    let t = Instant::now();
                    let owned = envs.acquire(WorkId::new(format!("w{i}"))).await.unwrap();
                    drop(std::hint::black_box(owned));
                    d.push(t.elapsed());
                }
                report.time(
                    "L1",
                    "environment acquire + release (local)",
                    d,
                    "builds a fresh capability set each time",
                );
                let mut d = Vec::new();
                for _ in 0..opts.samples(30) {
                    let t = Instant::now();
                    let _ = LocalEnvironmentProvider::prepare(&root).await;
                    d.push(t.elapsed());
                }
                report.time(
                    "L1",
                    "environment provider prepare (resolves PAX)",
                    d,
                    "one subprocess: `pax --version`",
                );
            }
            Err(why) => report.skip(
                "environment acquire/release (local)",
                &format!("PAX unavailable: {why}"),
            ),
        }
    }
    {
        struct One(Arc<dyn WorkEnvironment>);
        #[async_trait::async_trait]
        impl EnvironmentProvider for One {
            fn isolation_capacity(&self) -> usize {
                1
            }
            async fn acquire(
                &self,
                _w: &WorkId,
            ) -> Result<Arc<dyn WorkEnvironment>, EnvironmentError> {
                Ok(self.0.clone())
            }
            fn release(&self, _w: &WorkId, _e: &EnvironmentId) {}
        }
        let envs = Environments::new(Arc::new(One(Arc::new(env))));
        let mut d = Vec::with_capacity(n);
        for i in 0..n {
            let t = Instant::now();
            let owned = envs.acquire(WorkId::new(format!("w{i}"))).await.unwrap();
            drop(std::hint::black_box(owned));
            d.push(t.elapsed());
        }
        report.time(
            "L0",
            "environment acquire + release (Environments bookkeeping)",
            d,
            "provider returns a prebuilt environment",
        );
    }

    // -- observation and evidence --
    let result = ExecutionResult {
        id: ExecutionId::new("e1"),
        status: ExecutionStatus::Success,
        output: "x".repeat(2000),
        receipt_id: Some("sha256:abc".into()),
    };
    let mut d = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        let _ = std::hint::black_box(ExecutionObserver.observe(std::hint::black_box(&result)));
        d.push(t.elapsed());
    }
    report.time(
        "L0",
        "observe: ExecutionResult -> Observation (2 KB)",
        d,
        "",
    );
    let observation = ExecutionObserver.observe(&result).unwrap();
    let mut d = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        let _ = std::hint::black_box(observation.render());
        d.push(t.elapsed());
    }
    report.time("L0", "observation render for the model (2 KB)", d, "");
    let agent = Agent::new(Synthetic::new(vec![]));
    let mut d = Vec::with_capacity(n);
    let mut e = Vec::with_capacity(n);
    for i in 0..n {
        let request =
            chip_core::CapabilityRequest::new(ExecutionId::new(format!("r{i}")), noop_id.clone());
        let mut obs = observation.clone();
        obs.execution_id = request.execution_id.clone();
        let t = Instant::now();
        let _ = std::hint::black_box(agent.record_evidence(&request, &obs));
        d.push(t.elapsed());
        let t = Instant::now();
        let _ = std::hint::black_box(agent.lookup_evidence(&request));
        e.push(t.elapsed());
    }
    report.time("L0", "evidence record", d, "");
    report.time("L0", "evidence lookup (hit)", e, "store grows to n entries");

    // -- event creation (representative: the events one executed step produces) --
    let work = WorkId::new("w");
    let mut d = Vec::with_capacity(n);
    for turn in 0..n {
        let t = Instant::now();
        let mut events: Vec<WorkEvent> = Vec::with_capacity(8);
        events.push(WorkEvent::DecisionStarted {
            work_id: work.clone(),
            turn,
        });
        events.push(WorkEvent::DecisionMade {
            work_id: work.clone(),
            turn,
            decision: "request project.read".into(),
        });
        events.push(WorkEvent::CapabilityRequested {
            work_id: work.clone(),
            turn,
            capability: noop_id.clone(),
        });
        std::hint::black_box(&events);
        d.push(t.elapsed());
    }
    report.time(
        "L0",
        "event construction (3 representative events)",
        d,
        "proxy: payload allocation only, see loop rows for events per step",
    );

    // -- goal evaluation over a growing trajectory --
    for len in [1usize, 10, 100, 1000] {
        let trajectory: Vec<Observation> = (0..len)
            .map(|i| Observation {
                execution_id: ExecutionId::new(format!("o{i}")),
                kind: ObservationKind::ExecutionCompleted,
                status: ExecutionStatus::Success,
                output: Some("{\"ok\":true}\nbody".into()),
                receipt_id: None,
            })
            .collect();
        let predicate = chip_cli::software_work::VerifiedChange;
        let mut d = Vec::with_capacity(n);
        for _ in 0..n.min(500) {
            let t = Instant::now();
            let _ = std::hint::black_box(
                predicate.satisfied_by_trajectory(std::hint::black_box(&trajectory)),
            );
            d.push(t.elapsed());
        }
        report.time(
            "L0",
            format!("goal evaluation over {len} observations"),
            d,
            "VerifiedChange::satisfied_by_trajectory",
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

// ---- section: loop (L0) ----------------------------------------------------------------------------

struct LoopRun {
    total: Duration,
    overhead: Duration,
    model: Duration,
    compute: Duration,
    events: usize,
    last_context_bytes: usize,
}

async fn l0_run(steps: usize) -> LoopRun {
    let mut replies: Vec<String> = (0..steps).map(|_| request_json(NOOP, "")).collect();
    replies.push(COMPLETE.into());
    let model = Synthetic::new(replies);
    let backend = Arc::new(Noop {
        executed: AtomicUsize::new(0),
    });
    let set = Arc::new(CapabilitySet::new().with(backend.clone()));
    let agent = Agent::with_model(model.clone(), "synthetic")
        .with_capabilities(set.clone())
        .with_executor(set)
        .with_observer(Arc::new(ExecutionObserver));
    let spec = WorkSpec::new(WorkId::new("l0"), WorkGoal::new("baseline"))
        .with_limits(WorkLimits {
            max_turns: steps + 2,
            max_executions: steps + 1,
        })
        .with_evidence_reuse_prohibited(CapabilityId::new(NOOP).unwrap());
    let report = agent
        .run_work(&spec, &NoLocalPolicy, &ModelDecisionBoundary)
        .await;

    // The counts the baseline is only meaningful with: nothing extra happened.
    let m = report.measurement();
    assert!(
        matches!(report.outcome, chip_core::WorkOutcome::Completed { .. }),
        "{:?}",
        report.outcome
    );
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        steps + 1,
        "one model call per escalation, no retries"
    );
    assert_eq!(m.model_calls as usize, steps + 1);
    assert_eq!(
        backend.executed.load(Ordering::SeqCst),
        steps,
        "one execution per executed request"
    );
    assert_eq!(m.executions as usize, steps);

    let overhead = report
        .latency
        .total
        .saturating_sub(report.latency.model + report.latency.compute);
    LoopRun {
        total: report.latency.total,
        overhead,
        model: report.latency.model,
        compute: report.latency.compute,
        events: report.events.len(),
        last_context_bytes: report.escalations.last().map_or(0, |c| c.bytes),
    }
}

async fn loop_l0(report: &mut Report, opts: &Opts) {
    let runs = opts.samples(40);
    for steps in [1usize, 5, 10, 25, 50] {
        let mut totals = Vec::new();
        let mut overheads = Vec::new();
        let mut per_step = Vec::new();
        let (mut events, mut bytes, mut model, mut compute) =
            (0, 0, Duration::ZERO, Duration::ZERO);
        for _ in 0..runs {
            let r = l0_run(steps).await;
            per_step.push(r.overhead / (steps as u32 + 1));
            totals.push(r.total);
            overheads.push(r.overhead);
            (events, bytes, model, compute) = (r.events, r.last_context_bytes, r.model, r.compute);
        }
        report.time(
            "L0",
            format!("work loop, {steps} steps: Chip overhead"),
            overheads,
            &format!("{events} events, last request {bytes} B"),
        );
        report.time(
            "L0",
            format!("work loop, {steps} steps: Chip overhead per decision"),
            per_step,
            "overhead / (steps+1 decisions)",
        );
        report.time(
            "L0",
            format!("work loop, {steps} steps: wall total"),
            totals,
            &format!(
                "synthetic model {:.0}us, execution {:.0}us in the last run",
                model.as_secs_f64() * 1e6,
                compute.as_secs_f64() * 1e6
            ),
        );
    }
    // The loop's memory: resident size before and after many runs.
    if let Some((before, _)) = memory(None) {
        for _ in 0..opts.samples(200) {
            l0_run(10).await;
        }
        if let Some((after, hwm)) = memory(None) {
            report.value(
                "L0",
                "memory: RSS growth over the runs above and 200 x 10-step works",
                "KiB",
                after as f64 - before as f64,
                &format!("RSS {after} KiB, peak {hwm} KiB"),
            );
        }
    }
}

// ---- section: real (L1) ----------------------------------------------------------------------------

async fn real(report: &mut Report, opts: &Opts) {
    let runs = opts.samples(20);

    // No PAX needed: list, search, read, write, then the model stops the work.
    let mut overhead = Vec::new();
    let mut compute = Vec::new();
    let mut note = String::new();
    for i in 0..runs {
        let root = fixture(&format!("real-{i}"));
        let env = LocalEnvironment::new(
            opaque_id(&root),
            &root,
            PaxExecutor::new(&root),
            EnvironmentDescription::default(),
        );
        let model = Synthetic::new(vec![
            request_json("project.list", "{\"path\":\".\"}"),
            request_json("project.search", "{\"query\":\"len\"}"),
            request_json("project.read", "{\"path\":\"src/lib.rs\"}"),
            request_json(
                "project.write",
                &format!(
                    "{{\"path\":\"src/lib.rs\",\"content\":{}}}",
                    serde_json::to_string(FIXTURE_NEW).unwrap()
                ),
            ),
            request_json("project.git.status", ""),
            BLOCK.into(),
        ]);
        let w = run_software_work_with_budget(
            WorkId::new("real"),
            model.clone(),
            "synthetic".into(),
            &env,
            "baseline",
            WorkLimits {
                max_turns: 12,
                max_executions: 8,
            },
            &chip_core::NoLocalPolicy,
            None,
        )
        .await;
        let m = w.report.measurement();
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            6,
            "one model call per escalation, no retries"
        );
        assert_eq!(m.executions, 5);
        overhead.push(
            w.report
                .latency
                .total
                .saturating_sub(w.report.latency.model + w.report.latency.compute),
        );
        compute.push(w.report.latency.compute);
        note = format!(
            "{} events, {} observations",
            w.report.events.len(),
            m.observations
        );
        let _ = std::fs::remove_dir_all(&root);
    }
    report.time(
        "L1",
        "real-capability loop (list, search, read, write, git status): Chip overhead",
        overhead,
        &note,
    );
    report.time(
        "L1",
        "real-capability loop: execution time (5 capabilities)",
        compute,
        "real filesystem and git",
    );

    // Full cycle with a real PAX: fix the project, verify it, the runtime completes the work.
    let probe = fixture("pax-probe");
    match real_pax(&probe).await {
        None => report.skip(
            "L1 full cycle with real PAX",
            "no usable PAX on PATH / PAX_BIN",
        ),
        Some(version) => {
            report.fact("pax", version);
            let mut overhead = Vec::new();
            let mut pax_time = Vec::new();
            let mut totals = Vec::new();
            let mut useful = String::new();
            for i in 0..opts.samples(5).min(5) {
                let root = fixture(&format!("cycle-{i}"));
                let env = LocalEnvironment::new(
                    opaque_id(&root),
                    &root,
                    PaxExecutor::new(&root),
                    EnvironmentDescription::default(),
                );
                let model = Synthetic::new(vec![
                    request_json("project.read", "{\"path\":\"src/lib.rs\"}"),
                    request_json(
                        "project.write",
                        &format!(
                            "{{\"path\":\"src/lib.rs\",\"content\":{}}}",
                            serde_json::to_string(FIXTURE_NEW).unwrap()
                        ),
                    ),
                    request_json("pax.test", ""),
                ]);
                let w = run_software_work_with_budget(
                    WorkId::new("cycle"),
                    model.clone(),
                    "synthetic".into(),
                    &env,
                    "make the tests pass",
                    WorkLimits {
                        max_turns: 8,
                        max_executions: 6,
                    },
                    &CompleteWhenVerified,
                    None,
                )
                .await;
                assert!(
                    w.verified,
                    "the goal was verified by PAX: {:?}",
                    w.report.outcome
                );
                assert_eq!(
                    model.calls.load(Ordering::SeqCst),
                    3,
                    "the runtime completes the work itself; no model call to finish"
                );
                overhead.push(
                    w.report
                        .latency
                        .total
                        .saturating_sub(w.report.latency.model + w.report.latency.compute),
                );
                pax_time.push(w.report.latency.compute);
                totals.push(w.report.latency.total);
                useful = format!(
                    "verified goals per model call = {:.2} (scripted model: validates the metric, says nothing about a real one)",
                    w.utility.verified_outputs as f64 / w.utility.model_calls.max(1) as f64
                );
                let _ = std::fs::remove_dir_all(&root);
            }
            report.time(
                "L1",
                "full cycle read, write, pax.test, evaluate: Chip overhead",
                overhead,
                &useful,
            );
            report.time(
                "L1",
                "full cycle: execution time (dominated by cargo through PAX)",
                pax_time,
                "first run compiles",
            );
            report.time("L1", "full cycle: wall total", totals, "");
        }
    }
    let _ = std::fs::remove_dir_all(&probe);
}

// ---- a mock model endpoint -------------------------------------------------------------------------

struct Mock {
    addr: std::net::SocketAddr,
    first: Arc<Mutex<Option<Instant>>>,
    requests: Arc<AtomicUsize>,
}

impl Mock {
    fn reset(&self) {
        *self.first.lock().unwrap() = None;
        self.requests.store(0, Ordering::SeqCst);
    }
    fn endpoint(&self) -> String {
        format!("http://{}/v1/chat/completions", self.addr)
    }
}

/// A chat-completions endpoint. `script(step)` is the reply for the request that has seen `step`
/// observations; `delay` stands in for model latency. One request per connection.
async fn mock_model(delay: Duration, script: fn(usize) -> &'static str) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let first = Arc::new(Mutex::new(None));
    let requests = Arc::new(AtomicUsize::new(0));
    let (f, r) = (first.clone(), requests.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let (f, r) = (f.clone(), r.clone());
            tokio::spawn(async move {
                let arrived = Instant::now();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                loop {
                    let Ok(n) = stream.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                {
                    let mut first = f.lock().unwrap();
                    first.get_or_insert(arrived);
                }
                r.fetch_add(1, Ordering::SeqCst);
                let body = String::from_utf8_lossy(&buf);
                let step = body.matches("Observation:\\nkind:").count();
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let content = serde_json::to_string(script(step)).unwrap();
                let payload = format!(
                    "{{\"id\":\"m\",\"choices\":[{{\"message\":{{\"role\":\"assistant\",\"content\":{content}}}}}],\"usage\":{{\"prompt_tokens\":100,\"completion_tokens\":20}}}}"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    Mock {
        addr,
        first,
        requests,
    }
}

/// project.list, then Git status twice, then stop.
fn list_git_git_block(step: usize) -> &'static str {
    match step {
        0 => {
            "{\"decision\":\"request_capability\",\"capability\":\"project.list\",\"inputs\":{\"path\":\".\"}}"
        }
        1 | 2 => "{\"decision\":\"request_capability\",\"capability\":\"project.git.status\"}",
        _ => BLOCK,
    }
}

fn block_now(_: usize) -> &'static str {
    BLOCK
}

/// list, read, then stop: two executions and three model calls per work.
fn list_read_block(step: usize) -> &'static str {
    match step {
        0 => {
            "{\"decision\":\"request_capability\",\"capability\":\"project.list\",\"inputs\":{\"path\":\".\"}}"
        }
        1 => {
            "{\"decision\":\"request_capability\",\"capability\":\"project.read\",\"inputs\":{\"path\":\"src/lib.rs\"}}"
        }
        _ => BLOCK,
    }
}

/// One HTTP exchange. Returns (status, body).
async fn http(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

async fn submit(addr: std::net::SocketAddr) -> String {
    let (status, body) = http(addr, "POST", "/v1/work", "{\"goal\":\"baseline\"}").await;
    assert_eq!(status, 202, "{body}");
    serde_json::from_str::<serde_json::Value>(&body).unwrap()["work_id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Polls until the work has ended; returns its status document.
async fn await_end(addr: std::net::SocketAddr, id: &str) -> serde_json::Value {
    loop {
        let (_, body) = http(addr, "GET", &format!("/v1/work/{id}"), "").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        if !matches!(v["status"].as_str(), Some("queued") | Some("running")) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

// ---- section: process ------------------------------------------------------------------------------

/// A `pax` that is on PATH: the real one if there is one, else an identity-only stand-in. The
/// stand-in lets `chip` start; it runs no tests, and the report says which was used.
fn pax_path() -> (std::ffi::OsString, &'static str) {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let real = std::process::Command::new("pax")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| {
            String::from_utf8_lossy(&o.stdout).starts_with("pax 0.")
                || String::from_utf8_lossy(&o.stdout).starts_with("pax ")
        })
        .is_some();
    if real {
        return (current, "real PAX");
    }
    let dir = temp_dir("fakepax");
    let script = dir.join("pax");
    std::fs::write(&script, "#!/bin/sh\necho \"pax 9.9.9\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut paths = vec![dir];
    paths.extend(std::env::split_paths(&current));
    (
        std::env::join_paths(paths).unwrap(),
        "identity-only PAX stand-in",
    )
}

async fn process(report: &mut Report, opts: &Opts) {
    let Some(chip) = option_env!("CARGO_BIN_EXE_chip") else {
        report.skip("process metrics", "the chip binary path is unknown");
        return;
    };
    let (path, pax_kind) = pax_path();
    report.fact("pax for process runs", pax_kind);

    // Binary startup.
    let mut d = Vec::new();
    for _ in 0..opts.samples(50) {
        let t = Instant::now();
        let out = std::process::Command::new(chip)
            .arg("--version")
            .output()
            .unwrap();
        d.push(t.elapsed());
        assert!(out.status.success());
    }
    report.time("process", "chip --version (process start to exit)", d, "");

    // `chip work`: process start until the first model request arrives, and until exit.
    let mock = mock_model(Duration::ZERO, block_now).await;
    let project = fixture("proc-work");
    let (mut to_model, mut to_exit) = (Vec::new(), Vec::new());
    for _ in 0..opts.samples(20) {
        mock.reset();
        let t = Instant::now();
        let mut child = tokio::process::Command::new(chip)
            .args(["work", "baseline goal"])
            .current_dir(&project)
            .env("PATH", &path)
            .env("CHIP_PROVIDER", "openai-compatible")
            .env("CHIP_MODEL", "bench")
            .env("CHIP_ENDPOINT", mock.endpoint())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let status = child.wait().await.unwrap();
        to_exit.push(t.elapsed());
        let first = mock
            .first
            .lock()
            .unwrap()
            .expect("chip work asked the model");
        to_model.push(first.duration_since(t));
        assert_eq!(
            mock.requests.load(Ordering::SeqCst),
            1,
            "chip work made exactly one model call: no retry, no extra call"
        );
        assert_eq!(
            status.code(),
            Some(1),
            "the work was blocked by the model, as scripted"
        );
    }
    report.time(
        "process",
        "chip work: process start to first model request",
        to_model,
        "startup overhead before the model is asked",
    );
    report.time(
        "process",
        "chip work: process start to exit (model answers instantly)",
        to_exit,
        "whole one-decision work",
    );

    // Subprocesses: how many times does a work start `pax` or `git`? Counting wrappers record each
    // invocation and then run the real tool.
    {
        let wrap = temp_dir("counting");
        let log = wrap.join("calls.log");
        let mut ok = true;
        for tool in ["pax", "git"] {
            let real = std::env::split_paths(&path)
                .map(|d| d.join(tool))
                .find(|p| p.is_file());
            match real {
                Some(real) => {
                    let script = wrap.join(tool);
                    std::fs::write(
                        &script,
                        format!(
                            "#!/bin/sh\necho {tool} >> '{}'\nexec '{}' \"$@\"\n",
                            log.display(),
                            real.display()
                        ),
                    )
                    .unwrap();
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                            .unwrap();
                    }
                }
                None => ok = false,
            }
        }
        if !ok {
            report.skip("subprocess counts", "pax or git not found on PATH");
        } else {
            let mut counted = vec![wrap.clone()];
            counted.extend(std::env::split_paths(&path));
            let counted = std::env::join_paths(counted).unwrap();
            let git_project = fixture("proc-git");
            let _ = std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&git_project)
                .status();
            for (label, script, executions) in [
                (
                    "model stops immediately (0 executions)",
                    block_now as fn(usize) -> &'static str,
                    0,
                ),
                (
                    "project.list, git status, git status (3 executions)",
                    list_git_git_block,
                    3,
                ),
            ] {
                let mock = mock_model(Duration::ZERO, script).await;
                let _ = std::fs::remove_file(&log);
                let status = tokio::process::Command::new(chip)
                    .args(["work", "baseline goal"])
                    .current_dir(&git_project)
                    .env("PATH", &counted)
                    .env("CHIP_PROVIDER", "openai-compatible")
                    .env("CHIP_MODEL", "bench")
                    .env("CHIP_ENDPOINT", mock.endpoint())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .unwrap();
                let calls = std::fs::read_to_string(&log).unwrap_or_default();
                let count = |tool: &str| calls.lines().filter(|l| *l == tool).count();
                assert_eq!(status.code(), Some(1));
                assert_eq!(
                    mock.requests.load(Ordering::SeqCst),
                    executions + 1,
                    "one model call per decision"
                );
                report.value(
                    "process",
                    format!("chip work subprocesses, {label}: pax"),
                    "processes",
                    count("pax") as f64,
                    "",
                );
                report.value(
                    "process",
                    format!("chip work subprocesses, {label}: git"),
                    "processes",
                    count("git") as f64,
                    "",
                );
            }
            let _ = std::fs::remove_dir_all(&git_project);
        }
        let _ = std::fs::remove_dir_all(&wrap);
    }

    // `chip serve`.
    let port = free_port();
    let started = Instant::now();
    let mut child = tokio::process::Command::new(chip)
        .args(["serve", "--port", &port.to_string()])
        .current_dir(&project)
        .env("PATH", &path)
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "bench")
        .env("CHIP_ENDPOINT", mock.endpoint())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    loop {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_micros(500)).await;
    }
    report.time(
        "process",
        "chip serve: process start to accepting connections",
        vec![started.elapsed()],
        "one sample (each needs a new process)",
    );
    let pid = child.id();
    if let Some((rss, hwm)) = memory(pid) {
        report.value(
            "process",
            "chip serve: idle RSS",
            "KiB",
            rss as f64,
            &format!("peak {hwm} KiB"),
        );
    }
    let mut submit_ms = Vec::new();
    let mut to_first_call = Vec::new();
    let mut total = Vec::new();
    let mut reported = Vec::new();
    for _ in 0..opts.samples(30) {
        mock.reset();
        let t = Instant::now();
        let id = submit(addr).await;
        submit_ms.push(t.elapsed());
        let done = await_end(addr, &id).await;
        total.push(t.elapsed());
        if let Some(first) = *mock.first.lock().unwrap() {
            to_first_call.push(first.duration_since(t));
        }
        if let Some(ms) = done["scheduling"]["time_to_first_model_call_ms"].as_f64() {
            reported.push(Duration::from_secs_f64(ms / 1000.0));
        }
        assert_eq!(
            mock.requests.load(Ordering::SeqCst),
            1,
            "one decision, one model call"
        );
    }
    report.time(
        "service",
        "serve: POST /v1/work until 202 returned",
        submit_ms,
        "separate TCP connection per request",
    );
    report.time(
        "service",
        "serve: submit to first model request arriving at the endpoint",
        to_first_call,
        "request -> work-start latency",
    );
    if !reported.is_empty() {
        report.time(
            "service",
            "serve: scheduling.time_to_first_model_call_ms (as the service reports it)",
            reported,
            "",
        );
    }
    report.time(
        "service",
        "serve: submit to terminal state (model answers instantly)",
        total,
        "includes 1 ms status polling",
    );
    if let Some((rss, hwm)) = memory(pid) {
        report.value(
            "process",
            "chip serve: RSS after the works above",
            "KiB",
            rss as f64,
            &format!("peak {hwm} KiB; works are kept in memory"),
        );
    }
    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&project);
}

// ---- section: serve (concurrent throughput, in process) ---------------------------------------------

/// N isolated project directories, each its own environment. Real filesystem capabilities.
struct Isolated {
    slots: Vec<Arc<LocalEnvironment>>,
    free: Mutex<Vec<usize>>,
    taken: Mutex<BTreeMap<String, usize>>,
}

#[async_trait::async_trait]
impl EnvironmentProvider for Isolated {
    fn isolation_capacity(&self) -> usize {
        self.slots.len()
    }
    async fn acquire(&self, work: &WorkId) -> Result<Arc<dyn WorkEnvironment>, EnvironmentError> {
        let i = self
            .free
            .lock()
            .unwrap()
            .pop()
            .ok_or(EnvironmentError::AtCapacity {
                capacity: self.slots.len(),
            })?;
        self.taken
            .lock()
            .unwrap()
            .insert(work.as_str().to_string(), i);
        Ok(self.slots[i].clone())
    }
    fn release(&self, work: &WorkId, _environment: &EnvironmentId) {
        if let Some(i) = self.taken.lock().unwrap().remove(work.as_str()) {
            self.free.lock().unwrap().push(i);
        }
    }
}

async fn serve_throughput(report: &mut Report, opts: &Opts) {
    let works = opts.samples(60).max(12);
    for (delay_ms, label) in [
        (0u64, "model answers instantly"),
        (25, "model takes 25 ms per call"),
    ] {
        let mock = mock_model(Duration::from_millis(delay_ms), list_read_block).await;
        for concurrency in [1usize, 2, 4, 8, 16] {
            let dirs: Vec<PathBuf> = (0..concurrency)
                .map(|i| fixture(&format!("srv-{delay_ms}-{concurrency}-{i}")))
                .collect();
            let slots: Vec<Arc<LocalEnvironment>> = dirs
                .iter()
                .map(|d| {
                    Arc::new(LocalEnvironment::new(
                        opaque_id(d),
                        d,
                        PaxExecutor::new(d),
                        EnvironmentDescription::default(),
                    ))
                })
                .collect();
            let provider = Arc::new(Isolated {
                free: Mutex::new((0..slots.len()).collect()),
                slots,
                taken: Mutex::default(),
            });
            let selection = Selection {
                provider: Some("openai-compatible".into()),
                model: Some("bench".into()),
                endpoint: Some(mock.endpoint()),
            };
            let runtime = Arc::new(WorkRuntime::prepare(&selection, None).expect("runtime"));
            let service = Service::new(
                runtime,
                Arc::new(Environments::new(provider)),
                true,
                Capacity {
                    max_concurrent: concurrency,
                    max_queued: 1024,
                },
            )
            .expect("service");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(chip_cli::service::run(listener, service));
            mock.reset();

            let before = memory(None);
            let t0 = Instant::now();
            let mut tasks = Vec::new();
            for _ in 0..works {
                tasks.push(tokio::spawn(async move {
                    let t = Instant::now();
                    let id = submit(addr).await;
                    let done = await_end(addr, &id).await;
                    (t.elapsed(), done)
                }));
            }
            let mut latencies = Vec::new();
            let mut waits = Vec::new();
            for task in tasks {
                let (elapsed, done) = task.await.unwrap();
                assert_eq!(done["lifecycle"], "blocked", "{done}");
                latencies.push(elapsed);
                if let Some(ms) = done["scheduling"]["queue_wait_ms"].as_f64() {
                    waits.push(Duration::from_secs_f64(ms / 1000.0));
                }
            }
            let wall = t0.elapsed();
            assert_eq!(
                mock.requests.load(Ordering::SeqCst),
                works * 3,
                "three model calls per work, nothing extra"
            );
            latencies.sort();
            let throughput = works as f64 / wall.as_secs_f64();
            report.value(
                "service",
                format!("concurrency {concurrency:>2}, {label}: throughput"),
                "works/s",
                throughput,
                &format!(
                    "{works} works; submit->end p50 {:.1} ms, p95 {:.1} ms",
                    percentile(&latencies, 0.5).as_secs_f64() * 1e3,
                    percentile(&latencies, 0.95).as_secs_f64() * 1e3
                ),
            );
            if let (Some((b, _)), Some((a, hwm))) = (before, memory(None)) {
                report.value(
                    "service",
                    format!("concurrency {concurrency:>2}, {label}: RSS change over the batch"),
                    "KiB",
                    a as f64 - b as f64,
                    &format!("RSS {a} KiB, process peak {hwm} KiB"),
                );
            }
            server.abort();
            for d in dirs {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }
}

// ---- section: realmodel (L2) -----------------------------------------------------------------------

async fn real_model(report: &mut Report, _opts: &Opts) {
    if std::env::var("CHIP_BENCH_REAL_MODEL").ok().as_deref() != Some("1") {
        report.skip("L2 real model", "set CHIP_BENCH_REAL_MODEL=1 with CHIP_PROVIDER, CHIP_MODEL and CHIP_ENDPOINT (and CHIP_API_KEY if needed)");
        return;
    }
    let probe = fixture("l2-probe");
    if real_pax(&probe).await.is_none() {
        report.skip("L2 real model", "needs a real PAX to verify the goal");
        return;
    }
    let runtime = match WorkRuntime::prepare(&Selection::default(), None) {
        Ok(r) => r,
        Err(why) => {
            report.skip("L2 real model", &why);
            return;
        }
    };
    let root = fixture("l2");
    let env = LocalEnvironment::new(
        opaque_id(&root),
        &root,
        PaxExecutor::new(&root),
        EnvironmentDescription::default(),
    );
    let (w, _) = runtime
        .run(WorkId::new("l2"), "Add a function `canonical(s: &str) -> String` to src/lib.rs that sorts the `&`-separated pairs, so the project's tests pass.", WorkLimits { max_turns: 12, max_executions: 8 }, None, &env)
        .await;
    let m = w.report.measurement();
    let overhead = w
        .report
        .latency
        .total
        .saturating_sub(w.report.latency.model + w.report.latency.compute);
    report.fact(
        "L2 provider",
        format!("{} / {}", runtime.identity.provider, runtime.identity.model),
    );
    report.value(
        "L2",
        "real model: total",
        "ms",
        w.report.latency.total.as_secs_f64() * 1e3,
        &format!("outcome {:?}", w.report.outcome),
    );
    report.value(
        "L2",
        "real model: time in the model",
        "ms",
        w.report.latency.model.as_secs_f64() * 1e3,
        &format!("{} calls, {:?} tokens", m.model_calls, m.model_tokens),
    );
    report.value(
        "L2",
        "real model: time in execution",
        "ms",
        w.report.latency.compute.as_secs_f64() * 1e3,
        "",
    );
    report.value(
        "L2",
        "real model: Chip overhead",
        "ms",
        overhead.as_secs_f64() * 1e3,
        "total - model - execution",
    );
    report.value(
        "L2",
        "real model: verified goals per model call",
        "ratio",
        w.utility.verified_outputs as f64 / w.utility.model_calls.max(1) as f64,
        &format!(
            "verified={} invalid_decisions={} wrong_valid={} recoveries={} executions={}",
            w.verified,
            w.utility.invalid_decisions,
            w.utility.wrong_valid_decisions,
            w.utility.recoveries,
            w.utility.executions
        ),
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&probe);
}

// ---- main ------------------------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = Opts {
        quick: args.iter().any(|a| a == "--quick"),
        only: args
            .iter()
            .position(|a| a == "--only")
            .and_then(|i| args.get(i + 1))
            .cloned(),
    };
    if let Some(only) = &opts.only {
        assert!(
            SECTIONS.contains(&only.as_str()),
            "unknown section {only}; one of {SECTIONS:?}"
        );
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut report = Report::default();
    report.fact(
        "cpus",
        std::thread::available_parallelism()
            .map_or(0, |n| n.get())
            .to_string(),
    );
    if let Ok(cpu) = std::fs::read_to_string("/proc/cpuinfo") {
        if let Some(model) = cpu
            .lines()
            .find(|l| l.starts_with("model name"))
            .and_then(|l| l.split(':').nth(1))
        {
            report.fact("cpu", model.trim());
        }
    }
    report.fact(
        "profile",
        if cfg!(debug_assertions) {
            "debug (numbers are NOT representative)"
        } else {
            "optimized"
        },
    );
    report.fact(
        "samples",
        if opts.quick {
            "quick (reduced)"
        } else {
            "full"
        },
    );
    report.fact(
        "model for L0/L1",
        "synthetic, in process, answers instantly",
    );
    if let Some((rss, hwm)) = memory(None) {
        report.fact("harness RSS at start (KiB)", format!("{rss} (peak {hwm})"));
    }

    rt.block_on(async {
        if opts.wants("parts") {
            parts(&mut report, &opts).await;
        }
        if opts.wants("loop") {
            loop_l0(&mut report, &opts).await;
        }
        if opts.wants("real") {
            real(&mut report, &opts).await;
        }
        if opts.wants("process") {
            process(&mut report, &opts).await;
        }
        if opts.wants("serve") {
            serve_throughput(&mut report, &opts).await;
        }
        if opts.wants("realmodel") {
            real_model(&mut report, &opts).await;
        }
    });

    report.print();
    let out =
        std::env::var("CHIP_BENCH_JSON").unwrap_or_else(|_| "target/chip-baseline.json".into());
    if let Some(parent) = Path::new(&out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::File::create(&out).and_then(|mut f| f.write_all(report.json().as_bytes())) {
        Ok(()) => println!("\nwrote {out}"),
        Err(e) => eprintln!("could not write {out}: {e}"),
    }
}
