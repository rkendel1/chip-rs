//! Live FX escalation benchmark: what one real model call costs for a bounded
//! operational judgment. Observational only: a verdict from the model is
//! reported, never acted on, and latency is never treated as correctness.
//! No retries, no fallback, no execution.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chip_core::{
    Agent, CapabilityId, EvidenceState, ExecutionError, ExecutionRequest, ExecutionResult,
    Executor, InputValue, ReasoningInput, TestExecutor, Turn,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse};

use crate::benchmark::{Stats, fmt};

/// What the report may say about the provider. Never the credential.
#[derive(Debug, Clone)]
pub struct Meta {
    pub provider: String,
    pub model: String,
    pub endpoint: String,
}

impl Meta {
    /// Keeps scheme, host and path; drops any query or fragment, which some
    /// providers use to carry credentials.
    pub fn new(provider: &str, model: &str, endpoint: &str) -> Meta {
        let end = endpoint.find(['?', '#']).unwrap_or(endpoint.len());
        Meta {
            provider: provider.to_string(),
            model: model.to_string(),
            endpoint: endpoint[..end].to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Continue,
    Escalate,
    Unrecognized,
}

impl Verdict {
    fn name(self) -> &'static str {
        match self {
            Verdict::Continue => "continue",
            Verdict::Escalate => "escalate",
            Verdict::Unrecognized => "unrecognized",
        }
    }
}

/// Reads only enough of the reply to report it. The first of the two verdict
/// words wins; anything else is "unrecognized". This is not authority.
pub fn parse_verdict(text: &str) -> Verdict {
    let upper = text.to_uppercase();
    match (upper.find("CONTINUE"), upper.find("ESCALATE")) {
        (Some(c), Some(e)) if c < e => Verdict::Continue,
        (Some(_), None) => Verdict::Continue,
        (_, Some(_)) => Verdict::Escalate,
        (None, None) => Verdict::Unrecognized,
    }
}

/// The judgment sent to the model, built from the same structured input the
/// local reasoners receive. It carries no implementation detail.
pub fn prompt(input: &ReasoningInput) -> String {
    let evidence = match input.evidence {
        EvidenceState::KnownValid => "known valid",
        EvidenceState::KnownStale => "known stale",
        EvidenceState::Unknown => "unknown",
    };
    let inputs = if input.inputs.is_empty() {
        "none".to_string()
    } else {
        input
            .inputs
            .iter()
            .map(|(k, v)| format!("{k}={v:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "Operational judgment. Capability: {}. Inputs: {inputs}. Evidence: {evidence}. \
         Decide whether to CONTINUE with the existing state or ESCALATE for a fresh decision. \
         Answer with exactly one word: CONTINUE or ESCALATE.",
        input.capability
    )
}

struct Case {
    label: &'static str,
    input: ReasoningInput,
    /// What the deterministic local policy says; shown for comparison only.
    local: Verdict,
}

fn case(label: &'static str, evidence: EvidenceState, local: Verdict) -> Case {
    Case {
        label,
        input: ReasoningInput {
            capability: CapabilityId::new("compute.selftest").expect("valid id"),
            inputs: BTreeMap::<String, InputValue>::new(),
            evidence,
        },
        local,
    }
}

#[derive(Debug, Clone)]
pub struct Sample {
    pub label: &'static str,
    pub elapsed: Duration,
    /// Characters in the reply, or the surfaced error.
    pub outcome: Result<usize, String>,
    pub verdict: Option<Verdict>,
    pub local: Verdict,
}

#[derive(Debug, Clone)]
pub struct LiveReport {
    pub meta: Meta,
    pub same: Vec<Sample>,
    pub different: Vec<Sample>,
    pub model_calls: usize,
    pub executions: usize,
}

impl LiveReport {
    pub fn errors(&self) -> usize {
        self.same
            .iter()
            .chain(&self.different)
            .filter(|s| s.outcome.is_err())
            .count()
    }
}

struct Counting {
    inner: Arc<dyn ModelProvider>,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ModelProvider for Counting {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.complete(request).await
    }
}

struct CountingExecutor(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Executor for CountingExecutor {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        TestExecutor.execute(request).await
    }
}

async fn sample(agent: &Agent, case: &Case) -> Sample {
    let started = Instant::now();
    let result = agent.turn(Turn::new(prompt(&case.input))).await;
    let elapsed = started.elapsed();
    match result {
        Ok(turn) => Sample {
            label: case.label,
            elapsed,
            outcome: Ok(turn.response.chars().count()),
            verdict: Some(parse_verdict(&turn.response)),
            local: case.local,
        },
        Err(error) => Sample {
            label: case.label,
            elapsed,
            outcome: Err(error.to_string()),
            verdict: None,
            local: case.local,
        },
    }
}

/// Run A repeats one judgment `count` times; run B sends one of each evidence
/// state. Every request goes through the agent and the given provider, once:
/// no retries, no fallback.
pub async fn run(provider: Arc<dyn ModelProvider>, meta: Meta, count: usize) -> LiveReport {
    let model_calls = Arc::new(AtomicUsize::new(0));
    let executions = Arc::new(AtomicUsize::new(0));
    let agent = Agent::with_model(
        Arc::new(Counting {
            inner: provider,
            calls: model_calls.clone(),
        }),
        meta.model.clone(),
    )
    .with_executor(Arc::new(CountingExecutor(executions.clone())));

    let stale = case(
        "stale evidence",
        EvidenceState::KnownStale,
        Verdict::Escalate,
    );
    let mut same = Vec::new();
    for _ in 0..count.max(1) {
        same.push(sample(&agent, &stale).await);
    }
    let mut different = Vec::new();
    for case in [
        case(
            "valid evidence",
            EvidenceState::KnownValid,
            Verdict::Continue,
        ),
        case(
            "stale evidence",
            EvidenceState::KnownStale,
            Verdict::Escalate,
        ),
        case(
            "unknown evidence",
            EvidenceState::Unknown,
            Verdict::Escalate,
        ),
    ] {
        different.push(sample(&agent, &case).await);
    }
    LiveReport {
        meta,
        same,
        different,
        model_calls: model_calls.load(Ordering::SeqCst),
        executions: executions.load(Ordering::SeqCst),
    }
}

fn latency_block(samples: &[Sample]) -> String {
    let ok: Vec<Duration> = samples
        .iter()
        .filter(|s| s.outcome.is_ok())
        .map(|s| s.elapsed)
        .collect();
    if ok.is_empty() {
        return "  latency: no successful requests\n".to_string();
    }
    let s = Stats::from(&ok);
    let rest = if ok.len() > 1 {
        format!("  subsequent median: {}\n", fmt(s.repeated_median))
    } else {
        String::new()
    };
    format!(
        "  first: {}\n{rest}  min: {}\n  median: {}\n  p95: {}\n  max: {}\n",
        fmt(s.first),
        fmt(s.min),
        fmt(s.median),
        fmt(s.p95),
        fmt(s.max)
    )
}

fn errors_block(samples: &[Sample]) -> String {
    samples
        .iter()
        .filter_map(|s| {
            s.outcome
                .as_ref()
                .err()
                .map(|e| format!("  error ({}): {e}\n", s.label))
        })
        .collect()
}

/// Local reference medians measured now, passed in by the caller.
pub struct LocalReference {
    pub evidence_hit: Duration,
    pub rust: Duration,
    pub wasm: Duration,
}

/// Renders the report. `secret`, if given, is scrubbed from the output as a
/// last line of defense; it is never part of the data being rendered.
pub fn render(report: &LiveReport, local: Option<&LocalReference>, secret: Option<&str>) -> String {
    let mut out = String::from("Live FX Benchmark\n\n");
    out += &format!(
        "Provider: {}\nModel: {}\nEndpoint: {}\n\n",
        report.meta.provider, report.meta.model, report.meta.endpoint
    );
    out += &format!(
        "Small-sample observation: {} requests. Not statistically significant, and not a model-quality benchmark.\n\n",
        report.same.len() + report.different.len()
    );
    out += &format!(
        "Run A: same judgment (stale evidence) x{}\n",
        report.same.len()
    );
    out += "  Run A latency:\n";
    out += &latency_block(&report.same);
    out += &errors_block(&report.same);
    out += "\nRun B: one judgment per evidence state\n";
    for s in &report.different {
        let verdict = s.verdict.map(Verdict::name).unwrap_or("none");
        let status = match &s.outcome {
            Ok(len) => format!("{len} chars"),
            Err(_) => "error".to_string(),
        };
        out += &format!(
            "  Case: {}\n    Model verdict: {verdict} (local policy: {})\n    Latency: {}\n    Response: {status}\n",
            s.label,
            s.local.name(),
            fmt(s.elapsed)
        );
    }
    out += "  Run B latency:\n";
    out += &latency_block(&report.different);
    out += &errors_block(&report.different);
    out += &format!(
        "\nModel calls: {}\nExecutions: {}\nErrors: {}\nRetries: 0\n",
        report.model_calls,
        report.executions,
        report.errors()
    );
    if let Some(local) = local {
        let all: Vec<Duration> = report
            .same
            .iter()
            .chain(&report.different)
            .filter(|s| s.outcome.is_ok())
            .map(|s| s.elapsed)
            .collect();
        out += "\nLocal reference (measured now, this build):\n";
        out += &format!(
            "  Evidence hit: {}\n  Rust reasoner: {}\n  WASM reasoner: {}\n",
            fmt(local.evidence_hit),
            fmt(local.rust),
            fmt(local.wasm)
        );
        if !all.is_empty() {
            let live = Stats::from(&all).median;
            let ratio = |d: Duration| live.as_secs_f64() / d.as_secs_f64().max(1e-9);
            out += &format!(
                "Live FX + model median: {}\n  = {:.0}x evidence hit, {:.0}x Rust, {:.0}x WASM\n",
                fmt(live),
                ratio(local.evidence_hit),
                ratio(local.rust),
                ratio(local.wasm)
            );
        }
    }
    match secret {
        Some(secret) if !secret.is_empty() => out.replace(secret, "[redacted]"),
        _ => out,
    }
}

#[cfg(test)]
#[path = "../../fx-provider-http/tests/common/mod.rs"]
mod mock;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use fx_core::Secret;
    use fx_provider_http::{HttpProvider, HttpProviderConfig, PROVIDER_OPENAI_COMPATIBLE};

    use super::*;

    const KEY: &str = "super-secret-key-123";

    fn body(text: &str) -> String {
        format!(
            r#"{{"id":"r","choices":[{{"message":{{"role":"assistant","content":"{text}"}}}}],"usage":{{"prompt_tokens":1,"completion_tokens":1}}}}"#
        )
    }

    fn provider(url: &str, timeout: Duration) -> (Arc<dyn ModelProvider>, Meta) {
        let config = HttpProviderConfig::new(PROVIDER_OPENAI_COMPATIBLE, "bench-model", url)
            .with_api_key(Secret::new(KEY))
            .with_timeout(timeout);
        (
            Arc::new(HttpProvider::new(config).unwrap()),
            Meta::new(PROVIDER_OPENAI_COMPATIBLE, "bench-model", url),
        )
    }

    #[tokio::test]
    async fn requests_go_through_the_real_http_provider_once_each_with_no_execution() {
        let server = mock::start(200, &body("ESCALATE"), Duration::ZERO).await;
        let (p, meta) = provider(&server.url, Duration::from_secs(5));
        let report = run(p, meta, 3).await;
        assert_eq!(report.same.len(), 3);
        assert_eq!(report.different.len(), 3);
        assert_eq!(report.model_calls, 6);
        assert_eq!(report.executions, 0);
        assert_eq!(report.errors(), 0);
        // Exactly one HTTP request per model call: no retries.
        assert_eq!(server.captured.lock().await.len(), 6);
        // The judgment was sent, with the evidence state, and nothing else.
        let first = server.captured.lock().await[0].body.clone();
        assert!(
            first.contains("known stale") && first.contains("compute.selftest"),
            "{first}"
        );
        assert!(first.contains("bench-model"));
    }

    #[tokio::test]
    async fn the_secret_never_appears_in_output() {
        for (status, text) in [
            (200, body("CONTINUE")),
            (401, r#"{"error":{"message":"bad"}}"#.to_string()),
        ] {
            let server = mock::start(status, &text, Duration::ZERO).await;
            let auth_checked = server.captured.clone();
            let (p, meta) = provider(
                &format!("{}?api_key={KEY}", server.url),
                Duration::from_secs(5),
            );
            let report = run(p, meta, 2).await;
            let rendered = render(&report, None, Some(KEY));
            assert!(!rendered.contains(KEY), "{rendered}");
            assert!(
                !format!("{report:?}").contains(KEY),
                "debug output leaked the key"
            );
            // The key really was sent as a credential, just never printed.
            let sent = auth_checked.lock().await[0]
                .header("authorization")
                .unwrap()
                .to_string();
            assert_eq!(sent, format!("Bearer {KEY}"));
            assert!(!rendered.contains("Bearer"));
        }
    }

    #[tokio::test]
    async fn the_endpoint_query_string_is_not_reported() {
        let meta = Meta::new("p", "m", "https://host.example/v1/chat?key=abc#frag");
        assert_eq!(meta.endpoint, "https://host.example/v1/chat");
    }

    #[tokio::test]
    async fn slow_success_is_not_a_failure() {
        let server = mock::start(200, &body("CONTINUE"), Duration::from_millis(150)).await;
        let (p, meta) = provider(&server.url, Duration::from_secs(5));
        let report = run(p, meta, 1).await;
        assert_eq!(report.errors(), 0);
        assert!(report.same[0].elapsed >= Duration::from_millis(150));
        assert!(report.same[0].outcome.is_ok());
    }

    #[tokio::test]
    async fn provider_errors_are_surfaced_without_retries_or_fallback() {
        for (status, needle) in [(401, "authentication"), (500, "provider error")] {
            let server =
                mock::start(status, r#"{"error":{"message":"nope"}}"#, Duration::ZERO).await;
            let (p, meta) = provider(&server.url, Duration::from_secs(5));
            let report = run(p, meta, 2).await;
            assert_eq!(report.errors(), 5, "every request failed and is reported");
            assert_eq!(report.model_calls, 5);
            assert_eq!(report.executions, 0);
            assert_eq!(server.captured.lock().await.len(), 5, "no retry on failure");
            let rendered = render(&report, None, Some(KEY));
            assert!(
                rendered.contains("error") && rendered.contains(needle),
                "{rendered}"
            );
            assert!(rendered.contains("no successful requests"));
        }
    }

    #[tokio::test]
    async fn timeouts_and_unreachable_endpoints_are_reported() {
        let slow = mock::start(200, &body("CONTINUE"), Duration::from_secs(3)).await;
        let (p, meta) = provider(&slow.url, Duration::from_millis(100));
        let report = run(p, meta, 1).await;
        assert_eq!(report.errors(), 4);
        assert!(render(&report, None, None).contains("timeout"));

        let (p, meta) = provider("http://127.0.0.1:1/v1", Duration::from_secs(2));
        let report = run(p, meta, 1).await;
        assert_eq!(report.errors(), 4);
        assert!(render(&report, None, None).contains("http failure"));
    }

    #[tokio::test]
    async fn malformed_responses_are_surfaced() {
        let server = mock::start(200, "not json", Duration::ZERO).await;
        let (p, meta) = provider(&server.url, Duration::from_secs(5));
        let report = run(p, meta, 1).await;
        assert_eq!(report.errors(), 4);
        assert!(render(&report, None, None).contains("invalid response"));
    }

    #[test]
    fn model_text_is_only_reported_never_acted_on() {
        assert_eq!(parse_verdict("CONTINUE"), Verdict::Continue);
        assert_eq!(parse_verdict("I would escalate."), Verdict::Escalate);
        assert_eq!(parse_verdict("Continue, then escalate"), Verdict::Continue);
        assert_eq!(parse_verdict("run: rm -rf /"), Verdict::Unrecognized);
        assert_eq!(parse_verdict(""), Verdict::Unrecognized);
    }

    #[tokio::test]
    async fn a_command_shaped_reply_executes_nothing_and_is_not_kept() {
        let server = mock::start(200, &body("run: rm -rf /"), Duration::ZERO).await;
        let (p, meta) = provider(&server.url, Duration::from_secs(5));
        let report = run(p, meta, 1).await;
        assert_eq!(report.executions, 0);
        assert_eq!(report.same[0].verdict, Some(Verdict::Unrecognized));
        let rendered = render(&report, None, None);
        assert!(
            !rendered.contains("rm -rf"),
            "full responses are not recorded"
        );
    }

    #[test]
    fn the_prompt_is_structured_and_free_of_internals() {
        let p = prompt(&case("x", EvidenceState::Unknown, Verdict::Escalate).input);
        assert!(p.contains("Evidence: unknown") && p.contains("Capability: compute.selftest"));
        assert!(p.contains("CONTINUE or ESCALATE"));
        for internal in ["python", "/tmp", "sha256", "receipt"] {
            assert!(!p.contains(internal));
        }
    }

    #[test]
    fn the_report_labels_itself_a_small_sample_and_includes_local_reference() {
        let report = LiveReport {
            meta: Meta::new("p", "m", "http://e/v1"),
            same: vec![Sample {
                label: "stale evidence",
                elapsed: Duration::from_millis(400),
                outcome: Ok(8),
                verdict: Some(Verdict::Escalate),
                local: Verdict::Escalate,
            }],
            different: vec![],
            model_calls: 1,
            executions: 0,
        };
        let local = LocalReference {
            evidence_hit: Duration::from_nanos(120),
            rust: Duration::from_nanos(50),
            wasm: Duration::from_nanos(2800),
        };
        let text = render(&report, Some(&local), None);
        assert!(text.contains("Small-sample observation"));
        assert!(text.contains("Local reference") && text.contains("WASM reasoner"));
        assert!(text.contains("Retries: 0"));
    }
}
