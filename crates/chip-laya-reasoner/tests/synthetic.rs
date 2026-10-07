#![cfg(feature = "fixture")]
//! The real laya-decision load-and-infer path (candle, tokenizer, typed question) against a
//! tiny random-weight checkpoint generated on the fly. This proves the adapter and Laya agree
//! on the question and state format and that inference is local and deterministic. It says
//! nothing about the real model's quality: the weights are random.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chip_core::{
    Agent, Assessment, CapabilityId, CapabilityRequest, ExecutionId, ExecutionStatus,
    LocalReasoner, LocalReasoningResult, Observation, ObservationKind, ReasoningError,
    ReasoningInput, StateToken,
};
use chip_laya_reasoner::{LayaReasoner, LayaVerdict};
use chip_reasoning_corpus::{corpus, evaluate};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse};

fn reasoner(tag: &str) -> LayaReasoner {
    let dir = support::temp_dir(tag);
    support::write_tiny_checkpoint(&dir);
    LayaReasoner::from_dir(&dir).expect("the synthetic checkpoint loads")
}

#[test]
fn a_checkpoint_loads_from_an_explicit_local_directory_with_provenance() {
    let r = reasoner("load");
    let p = r.provenance();
    assert!(p.path.is_absolute() && p.path.join("model.safetensors").is_file());
    assert_eq!(p.backend, "candle-cpu");
    assert_eq!(
        p.upstream_version,
        chip_laya_reasoner::laya::UPSTREAM_VERSION
    );
    assert!(p.to_string().contains("revision not reported"));
}

#[test]
fn every_corpus_case_gets_a_valid_typed_answer_and_inference_is_deterministic() {
    let r = reasoner("corpus");
    for case in corpus() {
        let a = r
            .judge(&case.input)
            .unwrap_or_else(|e| panic!("{}: {e}", case.id));
        let b = r.judge(&case.input).unwrap();
        // Same model, same input: identical verdict and probabilities.
        assert_eq!(
            (a.verdict, &a.probabilities),
            (b.verdict, &b.probabilities),
            "{}",
            case.id
        );
        let sum: f32 = a.probabilities.values().sum();
        assert!(
            (sum - 1.0).abs() < 1e-2,
            "{}: probabilities sum to {sum}",
            case.id
        );
        assert!(
            a.confidence.is_finite()
                && a.answer_confidence.is_finite()
                && a.act_probability.is_finite()
        );
        assert!(a.input_tokens > 0);
        assert!(matches!(
            (&a.verdict, &a.result),
            (LayaVerdict::Continue, LocalReasoningResult::Continue { .. })
                | (LayaVerdict::Escalate, LocalReasoningResult::Escalate { .. })
        ));
    }
}

#[test]
fn the_corpus_replays_through_the_real_candle_path_with_normalized_verdicts_only() {
    let r = reasoner("replay");
    let cases = corpus();
    let result = evaluate(&r, &cases);
    assert_eq!(result.total, 32);
    assert!(
        result
            .cases
            .iter()
            .all(|c| c.error.is_none() && c.actual.is_some())
    );
    assert_eq!(result.correct + result.incorrect, 32);
    // The counting is real even though a random model's score is meaningless.
    assert_eq!(
        result.false_continues() + result.needless_escalations(),
        result.incorrect
    );
}

#[test]
fn input_changes_reach_the_model() {
    let r = reasoner("sensitivity");
    let cases = corpus();
    let first = r.judge(&cases[0].input).unwrap();
    let any_different = cases
        .iter()
        .any(|c| r.judge(&c.input).unwrap().probabilities != first.probabilities);
    assert!(
        any_different,
        "different structured inputs must produce different scores"
    );
}

#[test]
fn a_missing_model_is_unavailable_and_nothing_is_downloaded() {
    let dir = support::temp_dir("missing");
    // Nonexistent path, a bare name that would be a Hub id or checkpoint alias, and an empty directory.
    for path in [
        dir.join("nope"),
        "typed-decisions".into(),
        "convaiinnovations/laya".into(),
        dir.clone(),
    ] {
        let started = std::time::Instant::now();
        let error = LayaReasoner::from_dir(&path).unwrap_err();
        assert!(
            matches!(error, ReasoningError::Unavailable(_)),
            "{path:?}: {error:?}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "must not attempt a download"
        );
    }
}

#[test]
fn a_malformed_model_is_invalid_not_unavailable() {
    let dir = support::temp_dir("malformed");
    std::fs::write(dir.join("rl_agent_config.json"), "{}").unwrap();
    std::fs::write(dir.join("config.json"), "{}").unwrap();
    std::fs::write(dir.join("model.safetensors"), b"not a safetensors file").unwrap();
    std::fs::create_dir_all(dir.join("tokenizer")).unwrap();
    std::fs::write(dir.join("tokenizer/tokenizer.json"), "not json").unwrap();
    let error = LayaReasoner::from_dir(&dir).unwrap_err();
    assert!(
        matches!(error, ReasoningError::Failed(ref m) if m.contains("invalid Laya model")),
        "{error:?}"
    );
}

#[test]
fn a_subfolder_bundle_loads_too() {
    let root = support::temp_dir("bundle");
    support::write_tiny_checkpoint(&root.join("typed-decisions"));
    assert!(LayaReasoner::from_dir_subfolder(&root, "typed-decisions").is_ok());
    assert!(matches!(
        LayaReasoner::from_dir_subfolder(&root, "absent"),
        Err(ReasoningError::Unavailable(_))
    ));
}

// ---------- Chip integration through the real Laya path ----------

struct Counted {
    inner: LayaReasoner,
    calls: AtomicUsize,
}

impl LocalReasoner for Counted {
    fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.reason(input)
    }
}

struct RemoteModel(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl ModelProvider for RemoteModel {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(FxError::Provider(
            "the remote model must not be called".into(),
        ))
    }
}

#[test]
fn valid_evidence_bypasses_laya_and_stale_or_unknown_call_it_once_with_nothing_else() {
    let counted = Arc::new(Counted {
        inner: reasoner("agent"),
        calls: AtomicUsize::new(0),
    });
    let remote = Arc::new(AtomicUsize::new(0));
    // No executor is injected, so nothing can be executed.
    let agent =
        Agent::new(Arc::new(RemoteModel(remote.clone()))).with_local_reasoner(counted.clone());
    let request = |id: &str, cap: &str| {
        CapabilityRequest::new(ExecutionId::new(id), CapabilityId::new(cap).unwrap())
    };
    let (f1, f2) = (StateToken::new("F1"), StateToken::new("F2"));
    let fixture = |id: &str| Observation {
        execution_id: ExecutionId::new(id),
        kind: ObservationKind::ExecutionCompleted,
        status: ExecutionStatus::Success,
        output: None,
        receipt_id: None,
    };

    agent
        .record_evidence_under(&request("e1", "tests.run"), &fixture("e1"), &f1)
        .unwrap();
    assert!(matches!(
        agent
            .assess_evidence(&request("e2", "tests.run"), Some(&f1))
            .unwrap(),
        Assessment::Reuse(_)
    ));
    assert_eq!(
        counted.calls.load(Ordering::SeqCst),
        0,
        "valid evidence: zero Laya calls"
    );

    agent
        .assess_evidence(&request("e3", "tests.run"), Some(&f2))
        .unwrap(); // stale
    assert_eq!(counted.calls.load(Ordering::SeqCst), 1);
    agent
        .assess_evidence(&request("e4", "build.run"), Some(&f1))
        .unwrap(); // unknown
    assert_eq!(counted.calls.load(Ordering::SeqCst), 2);

    assert_eq!(
        remote.load(Ordering::SeqCst),
        0,
        "Laya never reaches the remote model"
    );
    // The verdicts left evidence exactly as it was.
    let stats = agent.evidence_stats();
    assert_eq!(stats.hits, 1);
    assert!(matches!(
        agent.lookup_valid_evidence(&request("e5", "build.run"), &f1),
        chip_core::EvidenceLookup::NotFound
    ));
}

#[test]
fn laya_inference_has_no_network_dependency() {
    // Point every proxy at a closed port: any network attempt would fail and break inference.
    let r = reasoner("offline");
    for variable in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        // SAFETY: set before inference; this test does not run other threads that read the environment.
        unsafe { std::env::set_var(variable, "http://127.0.0.1:9") };
    }
    let result = evaluate(&r, &corpus());
    assert!(result.cases.iter().all(|c| c.error.is_none()));
}

/// Utility, not a check: writes the synthetic checkpoint to `target/synthetic-laya` so the CLI
/// can be exercised end to end. Run with `-- --ignored write_synthetic_checkpoint`.
#[test]
#[ignore]
fn write_synthetic_checkpoint() {
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/synthetic-laya");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    support::write_tiny_checkpoint(&out);
}
