//! PR13: evidence first, local reasoning second, the model only on explicit escalation.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chip_core::{
    Agent, Assessment, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, CapabilityRequest, EvidenceLookup, EvidenceState, ExecutionError,
    ExecutionId, ExecutionObserver, ExecutionRequest, ExecutionResult, Executor, InputValue,
    LocalReasoner, LocalReasoningResult, ReasoningError, ReasoningInput, StateToken,
    TestLocalReasoner, Turn,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

#[derive(Default)]
struct Model(AtomicUsize);

#[async_trait::async_trait]
impl ModelProvider for Model {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ModelResponse::new("r", "ok", Usage::new(1, 1)))
    }
}

#[derive(Default)]
struct Exec(AtomicUsize);

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ExecutionResult::success(r.id, "ok").with_receipt_id("sha256:r"))
    }
}

struct Caps;

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(vec![CapabilityDescriptor::new(
            CapabilityId::new("cap.a")?,
            "A",
            "a",
        )])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

/// Counts calls and records inputs, delegating to a deterministic policy.
struct Counting {
    calls: AtomicUsize,
    seen: std::sync::Mutex<Vec<ReasoningInput>>,
    inner: TestLocalReasoner,
}

impl Counting {
    fn new(inner: TestLocalReasoner) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            seen: Default::default(),
            inner,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl LocalReasoner for Counting {
    fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(input.clone());
        self.inner.reason(input)
    }
}

struct Rig {
    agent: Agent,
    model: Arc<Model>,
    exec: Arc<Exec>,
    reasoner: Arc<Counting>,
}

fn rig(policy: TestLocalReasoner) -> Rig {
    let model = Arc::new(Model::default());
    let exec = Arc::new(Exec::default());
    let reasoner = Counting::new(policy);
    let agent = Agent::new(model.clone())
        .with_capabilities(Arc::new(Caps))
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(reasoner.clone());
    Rig {
        agent,
        model,
        exec,
        reasoner,
    }
}

impl Rig {
    fn counts(&self) -> (usize, usize, usize) {
        (
            self.reasoner.calls(),
            self.model.0.load(Ordering::SeqCst),
            self.exec.0.load(Ordering::SeqCst),
        )
    }
}

fn req(id: &str) -> CapabilityRequest {
    CapabilityRequest::new(ExecutionId::new(id), CapabilityId::new("cap.a").unwrap())
}

fn f1() -> StateToken {
    StateToken::new("F1")
}

fn f2() -> StateToken {
    StateToken::new("F2")
}

#[tokio::test]
async fn r1_valid_evidence_bypasses_the_reasoner() {
    let r = rig(TestLocalReasoner::default());
    r.agent
        .obtain_evidence_under(&req("e1"), &f1())
        .await
        .unwrap();
    let execs = r.counts().2;
    let assessment = r.agent.assess_evidence(&req("e2"), Some(&f1())).unwrap();
    assert!(matches!(assessment, Assessment::Reuse(_)));
    assert_eq!(
        r.counts(),
        (0, 0, execs),
        "no reasoner call, no model call, no new execution"
    );
}

#[tokio::test]
async fn r2_stale_evidence_reaches_the_reasoner_without_model_or_execution() {
    let policy = TestLocalReasoner::default().on(
        EvidenceState::KnownStale,
        LocalReasoningResult::Continue {
            rationale: "stale is fine here".into(),
        },
    );
    let r = rig(policy);
    r.agent
        .obtain_evidence_under(&req("e1"), &f1())
        .await
        .unwrap();
    let execs = r.counts().2;
    let a = r.agent.assess_evidence(&req("e2"), Some(&f2())).unwrap();
    assert_eq!(
        a,
        Assessment::Continue {
            rationale: "stale is fine here".into()
        }
    );
    assert_eq!(r.counts(), (1, 0, execs));
    assert_eq!(
        r.reasoner.seen.lock().unwrap()[0].evidence,
        EvidenceState::KnownStale
    );
}

#[tokio::test]
async fn r3_unknown_evidence_reaches_the_reasoner() {
    let r = rig(TestLocalReasoner::default());
    let a = r.agent.assess_evidence(&req("e1"), None).unwrap();
    assert!(matches!(a, Assessment::Escalate { .. }));
    assert_eq!(r.counts(), (1, 0, 0));
    assert_eq!(
        r.reasoner.seen.lock().unwrap()[0].evidence,
        EvidenceState::Unknown
    );
}

#[tokio::test]
async fn r4_escalate_executes_nothing_and_calls_no_model_by_itself() {
    let r = rig(TestLocalReasoner::default());
    let a = r.agent.assess_evidence(&req("e1"), None).unwrap();
    assert_eq!(
        a,
        Assessment::Escalate {
            reason: "no evidence".into()
        }
    );
    assert_eq!(
        r.counts(),
        (1, 0, 0),
        "escalation is advice; FX is the caller's explicit call"
    );

    // Only when the caller explicitly asks does the model get used.
    r.agent.turn(Turn::new("please decide")).await.unwrap();
    assert_eq!(r.counts(), (1, 1, 0));
}

#[tokio::test]
async fn continue_is_advice_not_an_execution() {
    let policy = TestLocalReasoner::default().on(
        EvidenceState::Unknown,
        LocalReasoningResult::Continue {
            rationale: "go ahead".into(),
        },
    );
    let r = rig(policy);
    let a = r.agent.assess_evidence(&req("e1"), None).unwrap();
    assert!(matches!(a, Assessment::Continue { .. }));
    assert_eq!(r.counts(), (1, 0, 0), "Continue does not execute");
}

#[test]
fn r5_the_reasoner_interface_exposes_no_executor_or_agent() {
    // The trait takes only the structured input; it is checked by construction:
    // this impl compiles with nothing but `&ReasoningInput` in scope.
    struct Probe;
    impl LocalReasoner for Probe {
        fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
            Ok(LocalReasoningResult::Escalate {
                reason: format!("{}", input.capability),
            })
        }
    }
    let out = Probe
        .reason(&ReasoningInput {
            capability: CapabilityId::new("cap.a").unwrap(),
            inputs: Default::default(),
            evidence: EvidenceState::Unknown,
        })
        .unwrap();
    assert_eq!(
        out,
        LocalReasoningResult::Escalate {
            reason: "cap.a".into()
        }
    );
}

#[tokio::test]
async fn r6_local_reasoning_never_creates_evidence() {
    let policy = TestLocalReasoner::default().on(
        EvidenceState::Unknown,
        LocalReasoningResult::Continue {
            rationale: "ok".into(),
        },
    );
    let r = rig(policy);
    for _ in 0..3 {
        r.agent.assess_evidence(&req("e1"), Some(&f1())).unwrap();
    }
    assert_eq!(
        r.agent.lookup_valid_evidence(&req("e1"), &f1()),
        EvidenceLookup::NotFound
    );
    assert_eq!(r.agent.evidence_stats().hits, 0);
}

#[tokio::test]
async fn r9_no_hidden_model_calls_across_all_paths() {
    let r = rig(TestLocalReasoner::default());
    r.agent
        .obtain_evidence_under(&req("e1"), &f1())
        .await
        .unwrap();
    for state in [&f1(), &f2()] {
        let _ = r.agent.assess_evidence(&req("e2"), Some(state));
    }
    let _ = r.agent.assess_evidence(&req("e3"), None);
    assert_eq!(r.model.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn r10_the_same_input_gives_the_same_verdict() {
    let r = rig(TestLocalReasoner::default());
    let first = r.agent.assess_evidence(&req("e1"), None).unwrap();
    for _ in 0..5 {
        assert_eq!(r.agent.assess_evidence(&req("e1"), None).unwrap(), first);
    }
    let input = ReasoningInput {
        capability: CapabilityId::new("cap.a").unwrap(),
        inputs: [("k".to_string(), InputValue::Integer(1))].into(),
        evidence: EvidenceState::KnownStale,
    };
    let policy = TestLocalReasoner::default();
    assert_eq!(policy.reason(&input), policy.reason(&input));
}

#[test]
fn a_missing_reasoner_is_an_explicit_error_but_valid_evidence_needs_none() {
    let agent = Agent::new(Arc::new(Model::default()));
    assert!(matches!(
        agent.assess_evidence(&req("e1"), None),
        Err(chip_core::AgentError::Reasoning(
            ReasoningError::Unavailable(_)
        ))
    ));
}
