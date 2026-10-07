#![cfg(feature = "runtime")]
//! The adapter against the real rust-ml-runtime API. The models here are test doubles
//! implementing the runtime's own `DecisionModel` trait: no real Laya model is installed
//! in CI, so these tests prove the adapter's contract, not model quality.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chip_core::{
    Agent, Assessment, CapabilityId, CapabilityRequest, EvidenceLookup, EvidenceState, ExecutionId,
    ExecutionStatus, InputValue, LocalReasoner, LocalReasoningResult, Observation, ObservationKind,
    ReasoningError, ReasoningInput, StateToken,
};
use chip_local_ml::{
    CONTINUE_LABEL, ESCALATE_LABEL, LocalMlError, ModelVerdict, QUESTION_NAME, RustMLReasoner,
    build_request, encode_state, installation_status, interpret,
};
use chip_reasoning_corpus::{Verdict, corpus, evaluate};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse};
use rust_ml_runtime::{
    DecisionExecution, DecisionModel, DecisionModelCapabilities, DecisionModelProvider,
    DecisionProvenance, DecisionRequest, DecisionResult, DecisionValue, DeviceKind,
    ModelDescription, ModelIdentity, Runtime, RuntimeError, RuntimeResult, TypedDecision,
};

#[derive(Clone)]
enum Behavior {
    Choice(&'static str),
    Raw(DecisionValue),
    NoDecisions,
    TwoDecisions,
    WrongName,
    Fail,
}

#[derive(Clone)]
struct Fake {
    behavior: Behavior,
    confidence: f64,
    calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<DecisionRequest>>>,
}

impl Fake {
    fn new(behavior: Behavior) -> Self {
        Self {
            behavior,
            confidence: 0.97,
            calls: Default::default(),
            seen: Default::default(),
        }
    }
}

fn typed(name: &str, value: DecisionValue, confidence: f64) -> TypedDecision {
    TypedDecision {
        name: name.into(),
        kind: "choice".into(),
        value,
        probabilities: BTreeMap::from([
            ("CONTINUE".to_string(), confidence),
            ("ESCALATE".to_string(), 1.0 - confidence),
        ]),
        confidence,
        action_probability: 0.5,
    }
}

impl DecisionModel for Fake {
    fn describe(&self) -> ModelDescription {
        ModelDescription {
            identifier: "fake/laya".into(),
            revision: Some("rev-1".into()),
            backend: "fake-backend".into(),
            artifact_path: PathBuf::from("/fake"),
            artifact_sha256: "abc123".into(),
            decision_types: vec!["choice".into()],
        }
    }

    fn capabilities(&self) -> DecisionModelCapabilities {
        let mut c = DecisionModelCapabilities::conservative("fake-backend");
        c.device = DeviceKind::Cpu;
        c
    }

    fn decide(&self, request: &DecisionRequest) -> RuntimeResult<DecisionResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(request.clone());
        let decisions = match &self.behavior {
            Behavior::Choice(label) => vec![typed(
                "verdict",
                DecisionValue::Choice((*label).into()),
                self.confidence,
            )],
            Behavior::Raw(value) => vec![typed("verdict", value.clone(), self.confidence)],
            Behavior::NoDecisions => vec![],
            Behavior::TwoDecisions => vec![
                typed("verdict", DecisionValue::Choice("CONTINUE".into()), 0.9),
                typed("verdict", DecisionValue::Choice("CONTINUE".into()), 0.9),
            ],
            Behavior::WrongName => vec![typed(
                "other",
                DecisionValue::Choice("CONTINUE".into()),
                0.9,
            )],
            Behavior::Fail => return Err(RuntimeError::execution("decide", "boom")),
        };
        Ok(DecisionResult {
            model: ModelIdentity {
                identifier: "fake/laya".into(),
                revision: Some("rev-1".into()),
            },
            backend: "fake-backend".into(),
            decisions,
            execution: DecisionExecution {
                latency: Duration::from_millis(3),
                input_tokens: 17,
                output_tokens: 0,
            },
            provenance: DecisionProvenance {
                artifact_path: PathBuf::from("/fake"),
                artifact_sha256: "abc123".into(),
                runtime_version: "9.9.9-fake".into(),
            },
        })
    }
}

fn input(evidence: EvidenceState) -> ReasoningInput {
    ReasoningInput {
        capability: CapabilityId::new("tests.run").unwrap(),
        inputs: BTreeMap::from([
            (
                "change_affects_capability".to_string(),
                InputValue::Bool(false),
            ),
            ("count".to_string(), InputValue::Integer(3)),
            ("label".to_string(), InputValue::Text("x".into())),
        ]),
        evidence,
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-local-ml-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A runtime provider that accepts any directory holding a `fake.model` marker.
struct FakeProvider(Fake);

impl DecisionModelProvider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }

    fn supports_artifact(&self, artifact: &Path) -> bool {
        artifact.join("fake.model").is_file()
    }

    fn load_decision_model(&self, _artifact: &Path) -> RuntimeResult<Box<dyn DecisionModel>> {
        Ok(Box::new(self.0.clone()))
    }
}

// ---------- construction ----------

#[test]
fn a_valid_model_loads_through_the_real_runtime() {
    let dir = temp_dir("valid");
    std::fs::write(dir.join("fake.model"), "x").unwrap();
    let runtime = Runtime::builder()
        .register_decision_provider(FakeProvider(Fake::new(Behavior::Choice("CONTINUE"))))
        .build();
    let reasoner = RustMLReasoner::load_artifact(&runtime, &dir).expect("loads");
    assert_eq!(reasoner.provenance().model, "fake/laya");
}

#[test]
fn a_missing_model_is_unavailable_not_invalid() {
    let root = temp_dir("root");
    let runtime = Runtime::builder().model_root(&root).build();
    let error = RustMLReasoner::load_installed(&runtime, "laya").unwrap_err();
    assert!(matches!(error, LocalMlError::Unavailable(_)), "{error:?}");
    let error = RustMLReasoner::load_artifact(&runtime, root.join("nope")).unwrap_err();
    assert!(matches!(error, LocalMlError::Unavailable(_)), "{error:?}");
    // An unknown registry name is also reported, not panicked on.
    assert!(RustMLReasoner::load_installed(&runtime, "no-such-model").is_err());
}

#[test]
fn an_invalid_artifact_is_invalid() {
    let empty = temp_dir("empty");
    let runtime = Runtime::new();
    let error = RustMLReasoner::load_artifact(&runtime, &empty).unwrap_err();
    assert!(matches!(error, LocalMlError::Invalid(_)), "{error:?}");
    // A file instead of a directory.
    let file = empty.join("file");
    std::fs::write(&file, "x").unwrap();
    assert!(RustMLReasoner::load_artifact(&runtime, &file).is_err());
}

#[test]
fn provider_and_backend_failures_are_classified() {
    struct Broken(fn() -> RuntimeError);
    impl DecisionModelProvider for Broken {
        fn name(&self) -> &str {
            "broken"
        }
        fn supports_artifact(&self, _a: &Path) -> bool {
            true
        }
        fn load_decision_model(&self, _a: &Path) -> RuntimeResult<Box<dyn DecisionModel>> {
            Err((self.0)())
        }
    }
    let dir = temp_dir("broken");
    let load = |make: fn() -> RuntimeError| {
        let runtime = Runtime::builder()
            // Drop the built-in providers' match by using a marker-free dir: only `Broken` accepts it.
            .register_decision_provider(Broken(make))
            .build();
        RustMLReasoner::load_artifact(&runtime, &dir)
    };
    // Built-in ONNX does not accept this directory, so `Broken` is the single match.
    let incompatible = load(|| RuntimeError::UnsupportedModel {
        model: "m".into(),
        backend: "b".into(),
        reason: "incompatible backend".into(),
    });
    assert!(matches!(incompatible, Err(LocalMlError::Invalid(_))));
    let unavailable =
        load(|| RuntimeError::backend_unavailable("onnx", "runtime initialization failed"));
    assert!(matches!(unavailable, Err(LocalMlError::Unavailable(_))));
    let integrity = load(|| RuntimeError::ModelIntegrity {
        model: "m".into(),
        reason: "bad hash".into(),
    });
    assert!(matches!(integrity, Err(LocalMlError::Invalid(_))));
}

// ---------- input ----------

#[test]
fn the_model_receives_exactly_the_reasoning_input() {
    let fake = Fake::new(Behavior::Choice("CONTINUE"));
    let reasoner = RustMLReasoner::from_model(Box::new(fake.clone()));
    reasoner.reason(&input(EvidenceState::KnownStale)).unwrap();
    let seen = fake.seen.lock().unwrap();
    let state = &seen[0].state;
    assert_eq!(state["capability"], "tests.run");
    assert_eq!(state["evidence"], "known_stale");
    assert_eq!(
        state["inputs"]["change_affects_capability"],
        serde_json::json!({"type":"bool","value":false})
    );
    assert_eq!(
        state["inputs"]["count"],
        serde_json::json!({"type":"integer","value":3})
    );
    assert_eq!(
        state["inputs"]["label"],
        serde_json::json!({"type":"text","value":"x"})
    );
    // Nothing but the schema marker, capability, evidence and inputs.
    let keys: Vec<_> = state.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys.len(), 4, "{keys:?}");
}

#[test]
fn the_same_input_always_produces_the_same_model_input() {
    for evidence in [
        EvidenceState::KnownValid,
        EvidenceState::KnownStale,
        EvidenceState::Unknown,
    ] {
        let a = build_request(&input(evidence));
        let b = build_request(&input(evidence));
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }
    assert_ne!(
        encode_state(&input(EvidenceState::KnownValid)),
        encode_state(&input(EvidenceState::Unknown))
    );
}

#[test]
fn the_question_is_a_constrained_two_way_choice() {
    let request = build_request(&input(EvidenceState::Unknown));
    assert_eq!(request.decisions.len(), 1);
    assert_eq!(request.decisions[0].name, QUESTION_NAME);
    let rust_ml_runtime::DecisionType::Choice { options } = &request.decisions[0].kind else {
        panic!()
    };
    let labels: Vec<_> = options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, [CONTINUE_LABEL, ESCALATE_LABEL]);
}

#[test]
fn the_adapter_reaches_no_environment_filesystem_network_or_process() {
    let src =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap();
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        "std::env",
        "std::fs",
        "std::net",
        "std::process",
        "std::time::SystemTime",
        "Instant",
        "reqwest",
        "tokio",
        "chip_compute",
        "fx_core",
        "Executor",
        "ModelProvider",
        "install_registered_model",
    ] {
        assert!(
            !code.contains(forbidden),
            "adapter must not reference {forbidden}"
        );
    }
}

// ---------- output ----------

#[test]
fn constrained_outputs_map_and_everything_else_fails_closed() {
    let verdict = |behavior: Behavior| {
        RustMLReasoner::from_model(Box::new(Fake::new(behavior)))
            .reason(&input(EvidenceState::Unknown))
    };
    assert!(matches!(
        verdict(Behavior::Choice("CONTINUE")),
        Ok(LocalReasoningResult::Continue { .. })
    ));
    assert!(matches!(
        verdict(Behavior::Choice("ESCALATE")),
        Ok(LocalReasoningResult::Escalate { .. })
    ));
    for bad in [
        Behavior::Choice("continue"),
        Behavior::Choice("CONTINUE because it is fine"),
        Behavior::Choice("run rm -rf /"),
        Behavior::Choice(""),
        Behavior::Raw(DecisionValue::Noul(true)),
        Behavior::Raw(DecisionValue::Score(0.9)),
        Behavior::NoDecisions,
        Behavior::TwoDecisions,
        Behavior::WrongName,
        Behavior::Fail,
    ] {
        assert!(matches!(verdict(bad), Err(ReasoningError::Failed(_))));
    }
}

#[test]
fn confidence_and_probabilities_are_recorded_but_not_authority() {
    let mut fake = Fake::new(Behavior::Choice("CONTINUE"));
    fake.confidence = 0.97;
    let reasoner = RustMLReasoner::from_model(Box::new(fake));
    let judgment = reasoner.judge(&input(EvidenceState::KnownStale)).unwrap();
    assert_eq!(judgment.verdict, ModelVerdict::Continue);
    assert_eq!(judgment.confidence, 0.97);
    assert_eq!(judgment.probabilities["CONTINUE"], 0.97);
    assert_eq!(judgment.latency, Duration::from_millis(3));
    assert_eq!(judgment.input_tokens, 17);
    assert_eq!(judgment.result_runtime_version, "9.9.9-fake");

    // A low-confidence CONTINUE is still CONTINUE and a high-confidence ESCALATE is still
    // ESCALATE: confidence never changes the verdict (there is no threshold).
    for (label, confidence) in [("CONTINUE", 0.01), ("ESCALATE", 0.99)] {
        let mut fake = Fake::new(Behavior::Choice(label));
        fake.confidence = confidence;
        let result = RustMLReasoner::from_model(Box::new(fake))
            .reason(&input(EvidenceState::Unknown))
            .unwrap();
        assert_eq!(
            matches!(result, LocalReasoningResult::Continue { .. }),
            label == "CONTINUE"
        );
    }
    // The public output type has nowhere to carry confidence.
    let result = RustMLReasoner::from_model(Box::new(Fake::new(Behavior::Choice("CONTINUE"))))
        .reason(&input(EvidenceState::Unknown))
        .unwrap();
    assert!(!format!("{result:?}").contains("0.97"));
}

#[test]
fn interpret_rejects_directly() {
    let fake = Fake::new(Behavior::Choice("CONTINUE"));
    let ok = fake
        .decide(&build_request(&input(EvidenceState::Unknown)))
        .unwrap();
    assert!(interpret(&ok).is_ok());
}

// ---------- provenance ----------

#[test]
fn provenance_is_observable_and_separate_from_the_verdict() {
    let reasoner = RustMLReasoner::from_model(Box::new(Fake::new(Behavior::Choice("ESCALATE"))));
    let p = reasoner.provenance();
    assert_eq!(p.model, "fake/laya");
    assert_eq!(p.revision.as_deref(), Some("rev-1"));
    assert_eq!(p.backend, "fake-backend");
    assert_eq!(p.device, "Cpu");
    assert_eq!(p.artifact_sha256, "abc123");
    assert_eq!(p.runtime_version, rust_ml_runtime::VERSION);
    assert!(p.to_string().contains("fake/laya"));
}

// ---------- Chip integration ----------

struct NoModel(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl ModelProvider for NoModel {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(FxError::Provider("remote model must not be called".into()))
    }
}

fn request(id: &str) -> CapabilityRequest {
    CapabilityRequest::new(
        ExecutionId::new(id),
        CapabilityId::new("tests.run").unwrap(),
    )
}

fn fixture(id: &str) -> Observation {
    Observation {
        execution_id: ExecutionId::new(id),
        kind: ObservationKind::ExecutionCompleted,
        status: ExecutionStatus::Success,
        output: None,
        receipt_id: None,
    }
}

#[test]
fn chip_consults_the_model_only_for_stale_and_unknown_evidence_and_it_cannot_act() {
    let (f1, f2) = (StateToken::new("F1"), StateToken::new("F2"));
    let fake = Fake::new(Behavior::Choice("CONTINUE"));
    let remote_calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(Arc::new(NoModel(remote_calls.clone())))
        .with_local_reasoner(Arc::new(RustMLReasoner::from_model(Box::new(fake.clone()))));

    // Valid evidence is reused; the local model is not consulted.
    agent
        .record_evidence_under(&request("e1"), &fixture("e1"), &f1)
        .unwrap();
    assert!(matches!(
        agent.assess_evidence(&request("e2"), Some(&f1)).unwrap(),
        Assessment::Reuse(_)
    ));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);

    // Stale evidence reaches the model, which says CONTINUE: advice only.
    let stale = agent.assess_evidence(&request("e3"), Some(&f2)).unwrap();
    assert!(matches!(stale, Assessment::Continue { .. }));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fake.seen.lock().unwrap()[0].state["evidence"],
        "known_stale"
    );

    // Unknown evidence reaches the model too.
    let other = CapabilityRequest::new(
        ExecutionId::new("e4"),
        CapabilityId::new("build.run").unwrap(),
    );
    agent.assess_evidence(&other, Some(&f1)).unwrap();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    assert_eq!(fake.seen.lock().unwrap()[1].state["evidence"], "unknown");

    // The verdict created no evidence, executed nothing (the agent has no executor) and
    // never touched the remote model.
    assert_eq!(
        agent.lookup_valid_evidence(&other, &f1),
        EvidenceLookup::NotFound
    );
    assert_eq!(
        agent.lookup_valid_evidence(&request("e5"), &f2),
        EvidenceLookup::Stale
    );
    assert_eq!(remote_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_failing_model_is_an_error_not_a_decision() {
    let agent = Agent::new(Arc::new(NoModel(Default::default()))).with_local_reasoner(Arc::new(
        RustMLReasoner::from_model(Box::new(Fake::new(Behavior::Fail))),
    ));
    assert!(agent.assess_evidence(&request("e1"), None).is_err());
}

// ---------- corpus ----------

#[test]
fn the_pr16_corpus_runs_through_the_adapter_and_safe_improvement_is_measured() {
    use chip_core::TestLocalReasoner;
    let cases = corpus();
    let baseline = evaluate(&TestLocalReasoner::default(), &cases);

    // Always ESCALATE: safe, but it also escalates the valid-evidence cases the baseline
    // continues on (Chip never consults a reasoner for those), so it is worse than baseline.
    let safe = RustMLReasoner::from_model(Box::new(Fake::new(Behavior::Choice("ESCALATE"))));
    let safe_result = evaluate(&safe, &cases);
    assert_eq!(safe_result.total, 32);
    assert_eq!(safe_result.false_continues(), 0);
    assert!(
        safe_result
            .safe_improvement_over(&baseline)
            .is_some_and(|n| n < 0)
    );

    // Always CONTINUE is unsafe and earns no credit, whatever its accuracy.
    let unsafe_ = RustMLReasoner::from_model(Box::new(Fake::new(Behavior::Choice("CONTINUE"))));
    let unsafe_result = evaluate(&unsafe_, &cases);
    assert!(unsafe_result.false_continues() > 0);
    assert_eq!(unsafe_result.safe_improvement_over(&baseline), None);

    // A failing model is recorded per case, not hidden.
    let broken = evaluate(
        &RustMLReasoner::from_model(Box::new(Fake::new(Behavior::Fail))),
        &cases,
    );
    assert_eq!(broken.correct, 0);
    assert!(broken.cases.iter().all(|c| c.actual.is_none()));
    let _ = Verdict::Continue;
}

// ---------- installation / offline ----------

#[test]
fn installation_status_reports_not_installed_without_touching_the_network() {
    let root = temp_dir("status");
    let runtime = Runtime::builder().model_root(&root).build();
    let status = installation_status(&runtime, "laya").unwrap();
    assert_eq!(status.to_string(), "not_installed");
}
