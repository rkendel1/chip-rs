//! Local reasoning economics: what each stage of the decision hierarchy costs.
//!
//! Timings are informational. Correctness, call counts and the absence of
//! executions are asserted; no timing threshold is.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chip_core::{
    Agent, Assessment, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, CapabilityRequest, EvidenceState, ExecutionError, ExecutionId,
    ExecutionObserver, ExecutionRequest, ExecutionResult, Executor, InputValue, LocalReasoner,
    LocalReasoningResult, ReasoningError, ReasoningInput, StateToken, TestExecutor,
    TestLocalReasoner, Turn,
};
use chip_wasm_reasoner::{WasmLocalReasoner, benchmark_fixture};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

/// Latency summary over one measured path.
#[derive(Debug, Clone)]
pub struct Stats {
    pub first: Duration,
    pub min: Duration,
    pub median: Duration,
    pub p95: Duration,
    pub max: Duration,
    /// Median excluding the first invocation.
    pub repeated_median: Duration,
}

impl Stats {
    pub(crate) fn from(samples: &[Duration]) -> Stats {
        let percentile = |sorted: &[Duration], p: f64| -> Duration {
            let rank = ((sorted.len() as f64) * p).ceil() as usize;
            sorted[rank.clamp(1, sorted.len()) - 1]
        };
        let mut sorted = samples.to_vec();
        sorted.sort();
        let mut rest = samples[1..].to_vec();
        rest.sort();
        Stats {
            first: samples[0],
            min: sorted[0],
            median: percentile(&sorted, 0.5),
            p95: percentile(&sorted, 0.95),
            max: sorted[sorted.len() - 1],
            repeated_median: if rest.is_empty() {
                samples[0]
            } else {
                percentile(&rest, 0.5)
            },
        }
    }
}

/// One measured path with its asserted call accounting.
#[derive(Debug, Clone)]
pub struct Measured {
    pub name: &'static str,
    pub stats: Stats,
    pub model_calls: usize,
    pub reasoner_calls: usize,
    pub executions: usize,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub cases: usize,
    pub per_state: usize,
    pub wasm_compile: Duration,
    pub rust: Measured,
    pub wasm: Measured,
    pub fx: Measured,
    pub evidence_hit: Measured,
    pub evidence_stale_wasm: Measured,
}

struct Case {
    input: ReasoningInput,
    expected: LocalReasoningResult,
}

/// Deterministic matrix: the three evidence states interleaved, with input
/// shapes varied so serialization cost is exercised. Expected verdicts come from
/// the explicit default policy (valid continues; stale and unknown escalate).
fn workload(per_state: usize) -> Vec<Case> {
    let policy = TestLocalReasoner::default();
    let states = [
        EvidenceState::KnownValid,
        EvidenceState::KnownStale,
        EvidenceState::Unknown,
    ];
    (0..per_state * 3)
        .map(|i| {
            let evidence = states[i % 3];
            let mut inputs = std::collections::BTreeMap::new();
            match (i / 3) % 4 {
                1 => {
                    drop(inputs.insert("label".to_string(), InputValue::Text(format!("item-{i}"))))
                }
                2 => drop(inputs.insert("count".to_string(), InputValue::Integer(i as i64))),
                3 => {
                    inputs.insert("force".to_string(), InputValue::Bool(i % 2 == 0));
                    inputs.insert("label".to_string(), InputValue::Text("x".into()));
                }
                _ => {}
            }
            let input = ReasoningInput {
                capability: CapabilityId::new("compute.selftest").expect("valid id"),
                inputs,
                evidence,
            };
            let expected = policy.reason(&input).expect("policy is total");
            Case { input, expected }
        })
        .collect()
}

fn time_reasoner(
    name: &'static str,
    reasoner: &dyn LocalReasoner,
    cases: &[Case],
) -> Result<Measured, String> {
    let mut samples = Vec::with_capacity(cases.len());
    for case in cases {
        let started = Instant::now();
        let actual = reasoner
            .reason(&case.input)
            .map_err(|e| format!("{name}: {e}"))?;
        samples.push(started.elapsed());
        // Correctness before performance: a fast wrong verdict is a failure.
        if actual != case.expected {
            return Err(format!(
                "{name}: wrong verdict for {:?}: got {actual:?}, expected {:?}",
                case.input.evidence, case.expected
            ));
        }
    }
    Ok(Measured {
        name,
        stats: Stats::from(&samples),
        model_calls: 0,
        reasoner_calls: cases.len(),
        executions: 0,
    })
}

#[derive(Default)]
struct CountingModel(AtomicUsize);

#[async_trait::async_trait]
impl ModelProvider for CountingModel {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ModelResponse::new("bench", "escalated", Usage::new(1, 1)))
    }
}

struct CountingReasoner {
    inner: Arc<dyn LocalReasoner>,
    calls: AtomicUsize,
}

impl LocalReasoner for CountingReasoner {
    fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.reason(input)
    }
}

#[derive(Default)]
struct CountingExecutor(AtomicUsize);

#[async_trait::async_trait]
impl Executor for CountingExecutor {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        TestExecutor.execute(request).await
    }
}

struct Caps;

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(vec![CapabilityDescriptor::new(
            CapabilityId::new("compute.selftest")?,
            "Self Test",
            "Deterministic",
        )])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

/// Runs every path over the same workload, asserting correctness and accounting.
pub async fn run(per_state: usize) -> Result<Report, String> {
    if per_state == 0 {
        return Err("iterations per state must be positive".into());
    }
    let cases = workload(per_state);
    let n = cases.len();

    // A. Deterministic Rust (the same trait, no WASM).
    let rust = time_reasoner("Rust", &TestLocalReasoner::default(), &cases)?;

    // B. WASM, as it exists: a fresh fuel-bounded instance per judgment.
    let compile_started = Instant::now();
    let wasm_reasoner =
        WasmLocalReasoner::from_bytes(&benchmark_fixture()).map_err(|e| e.to_string())?;
    let wasm_compile = compile_started.elapsed();
    let wasm = time_reasoner("WASM", &wasm_reasoner, &cases)?;

    // C. FX escalation through the deterministic provider: structural overhead only.
    let model = Arc::new(CountingModel::default());
    let agent = Agent::new(model.clone());
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let started = Instant::now();
        let result = agent
            .turn(Turn::new("judge this"))
            .await
            .map_err(|e| e.to_string())?;
        samples.push(started.elapsed());
        if result.response != "escalated" {
            return Err("FX: unexpected response".into());
        }
    }
    let fx = Measured {
        name: "FX",
        stats: Stats::from(&samples),
        model_calls: model.0.load(Ordering::SeqCst),
        reasoner_calls: 0,
        executions: 0,
    };
    if fx.model_calls != n {
        return Err(format!(
            "FX: expected {n} model calls, saw {}",
            fx.model_calls
        ));
    }

    // D/E. The full fast path against evidence, then evidence stale -> WASM.
    let model = Arc::new(CountingModel::default());
    let executor = Arc::new(CountingExecutor::default());
    let reasoner = Arc::new(CountingReasoner {
        inner: Arc::new(wasm_reasoner),
        calls: AtomicUsize::new(0),
    });
    let agent = Agent::new(model.clone())
        .with_capabilities(Arc::new(Caps))
        .with_executor(executor.clone())
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(reasoner.clone());
    let request = CapabilityRequest::new(
        ExecutionId::new("bench-1"),
        CapabilityId::new("compute.selftest").map_err(|e| e.to_string())?,
    );
    let (f1, f2) = (StateToken::new("F1"), StateToken::new("F2"));
    agent
        .obtain_evidence_under(&request, &f1)
        .await
        .map_err(|e| e.to_string())?; // setup, not measured
    let setup_executions = executor.0.load(Ordering::SeqCst);

    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let started = Instant::now();
        let assessment = agent
            .assess_evidence(&request, Some(&f1))
            .map_err(|e| e.to_string())?;
        samples.push(started.elapsed());
        if !matches!(assessment, Assessment::Reuse(_)) {
            return Err("evidence hit: expected reuse".into());
        }
    }
    let evidence_hit = Measured {
        name: "Evidence (hit)",
        stats: Stats::from(&samples),
        model_calls: model.0.load(Ordering::SeqCst),
        reasoner_calls: reasoner.calls.load(Ordering::SeqCst),
        executions: executor.0.load(Ordering::SeqCst) - setup_executions,
    };
    if (
        evidence_hit.model_calls,
        evidence_hit.reasoner_calls,
        evidence_hit.executions,
    ) != (0, 0, 0)
    {
        return Err("evidence hit must cost no model call, reasoner call or execution".into());
    }

    let expected = LocalReasoningResult::Escalate {
        reason: "evidence is stale".into(),
    };
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let started = Instant::now();
        let assessment = agent
            .assess_evidence(&request, Some(&f2))
            .map_err(|e| e.to_string())?;
        samples.push(started.elapsed());
        if assessment
            != (Assessment::Escalate {
                reason: "evidence is stale".into(),
            })
        {
            return Err(format!(
                "evidence stale: wrong verdict, expected {expected:?}"
            ));
        }
    }
    let evidence_stale_wasm = Measured {
        name: "Evidence (stale) + WASM",
        stats: Stats::from(&samples),
        model_calls: model.0.load(Ordering::SeqCst),
        reasoner_calls: reasoner.calls.load(Ordering::SeqCst),
        executions: executor.0.load(Ordering::SeqCst) - setup_executions,
    };
    if (
        evidence_stale_wasm.model_calls,
        evidence_stale_wasm.reasoner_calls,
        evidence_stale_wasm.executions,
    ) != (0, n, 0)
    {
        return Err(
            "stale evidence: expected one reasoner call each, no model call, no execution".into(),
        );
    }

    Ok(Report {
        cases: n,
        per_state,
        wasm_compile,
        rust,
        wasm,
        fx,
        evidence_hit,
        evidence_stale_wasm,
    })
}

pub(crate) fn fmt(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns >= 1_000_000 {
        format!("{:.2} ms", ns as f64 / 1e6)
    } else if ns >= 1_000 {
        format!("{:.1} us", ns as f64 / 1e3)
    } else {
        format!("{ns} ns")
    }
}

fn section(m: &Measured) -> String {
    let s = &m.stats;
    format!(
        "{}:\n  first: {}\n  repeated median: {}\n  min: {}\n  median: {}\n  p95: {}\n  max: {}\n  model calls: {}\n  reasoner calls: {}\n  executions: {}\n",
        m.name,
        fmt(s.first),
        fmt(s.repeated_median),
        fmt(s.min),
        fmt(s.median),
        fmt(s.p95),
        fmt(s.max),
        m.model_calls,
        m.reasoner_calls,
        m.executions
    )
}

pub fn render(report: &Report) -> String {
    let ratio = |a: Duration, b: Duration| a.as_secs_f64() / b.as_secs_f64().max(1e-9);
    let mut out = String::from("Local Reasoning Benchmark\n\n");
    out += &format!(
        "Cases: {} ({} per evidence state, interleaved)\nWASM module compile (one time): {}\n\n",
        report.cases,
        report.per_state,
        fmt(report.wasm_compile)
    );
    for m in [
        &report.rust,
        &report.wasm,
        &report.fx,
        &report.evidence_hit,
        &report.evidence_stale_wasm,
    ] {
        out += &section(m);
        out.push('\n');
    }
    out += &format!(
        "Ratios of medians (informational):\n  WASM / Rust: {:.1}x\n  FX / WASM: {:.2}x (FX here is an instant deterministic provider: structural overhead only)\n  Evidence stale+WASM / Evidence hit: {:.1}x\n\n",
        ratio(report.wasm.stats.median, report.rust.stats.median),
        ratio(report.fx.stats.median, report.wasm.stats.median),
        ratio(
            report.evidence_stale_wasm.stats.median,
            report.evidence_hit.stats.median
        ),
    );
    out += "Notes:\n  - every verdict was checked against the expected policy before timing was recorded\n  - WASM uses a fresh fuel-bounded instance per call; instance reuse is intentionally not offered\n  - allocation measurement is omitted (no lightweight mechanism is in the workspace)\n  - capability availability is not part of ReasoningInput; Chip checks it before reasoning\n  - timings vary by machine and are not asserted anywhere\n";
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_path_is_correct_and_accounted_for() {
        let report = run(30).await.expect("benchmark should be correct");
        assert_eq!(report.cases, 90);
        assert_eq!((report.rust.model_calls, report.rust.executions), (0, 0));
        assert_eq!((report.wasm.model_calls, report.wasm.executions), (0, 0));
        assert_eq!((report.fx.model_calls, report.fx.executions), (90, 0));
        assert_eq!(
            (
                report.evidence_hit.model_calls,
                report.evidence_hit.reasoner_calls,
                report.evidence_hit.executions
            ),
            (0, 0, 0)
        );
        assert_eq!(
            (
                report.evidence_stale_wasm.model_calls,
                report.evidence_stale_wasm.reasoner_calls,
                report.evidence_stale_wasm.executions
            ),
            (0, 90, 0)
        );
    }

    #[test]
    fn rust_and_wasm_agree_on_every_case() {
        let wasm = WasmLocalReasoner::from_bytes(&benchmark_fixture()).unwrap();
        let rust = TestLocalReasoner::default();
        for case in workload(40) {
            assert_eq!(wasm.reason(&case.input).unwrap(), case.expected);
            assert_eq!(rust.reason(&case.input).unwrap(), case.expected);
        }
    }

    #[test]
    fn the_workload_is_deterministic_and_covers_every_state() {
        let (a, b) = (workload(12), workload(12));
        assert_eq!(a.len(), 36);
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.input, y.input);
            assert_eq!(x.expected, y.expected);
        }
        for state in [
            EvidenceState::KnownValid,
            EvidenceState::KnownStale,
            EvidenceState::Unknown,
        ] {
            assert_eq!(a.iter().filter(|c| c.input.evidence == state).count(), 12);
        }
        assert!(
            a.iter().any(|c| !c.input.inputs.is_empty()),
            "input shapes vary"
        );
    }

    #[test]
    fn a_wrong_verdict_fails_the_run() {
        struct AlwaysContinue;
        impl LocalReasoner for AlwaysContinue {
            fn reason(&self, _i: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
                Ok(LocalReasoningResult::Continue {
                    rationale: "evidence is valid".into(),
                })
            }
        }
        assert!(time_reasoner("Wrong", &AlwaysContinue, &workload(5)).is_err());
    }

    #[test]
    fn stats_are_ordered() {
        let samples: Vec<Duration> = (1..=100)
            .rev()
            .map(|n| Duration::from_nanos(n * 10))
            .collect();
        let s = Stats::from(&samples);
        assert!(s.min <= s.median && s.median <= s.p95 && s.p95 <= s.max);
        assert_eq!(s.first, Duration::from_nanos(1000));
        assert_eq!(s.min, Duration::from_nanos(10));
        assert_eq!(s.max, Duration::from_nanos(1000));
    }
}
