//! PR6: the model proposes, Chip validates, the executor executes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chip_core::{
    Agent, AgentDecision, CapabilityAvailability, CapabilityDescriptor, CapabilityError,
    CapabilityId, CapabilityInput, CapabilityProvider, CapabilityRequest, DecisionBoundary,
    DecisionError, DecisionInput, ExecutionError, ExecutionId, ExecutionRequest, ExecutionResult,
    Executor, InputValue, ScriptedDecision, Turn,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

struct Model(&'static str);

#[async_trait::async_trait]
impl ModelProvider for Model {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        Ok(ModelResponse::new("r1", self.0, Usage::new(1, 1)))
    }
}

#[derive(Default)]
struct Counting(AtomicUsize);

#[async_trait::async_trait]
impl Executor for Counting {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ExecutionResult::success(r.id, "ran"))
    }
}

struct Caps {
    availability: CapabilityAvailability,
    discoveries: AtomicUsize,
}

impl Caps {
    fn new(availability: CapabilityAvailability) -> Arc<Self> {
        Arc::new(Self {
            availability,
            discoveries: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        self.discoveries.fetch_add(1, Ordering::SeqCst);
        let mut d =
            CapabilityDescriptor::new(CapabilityId::new("test.op").unwrap(), "Op", "Test op");
        d.inputs.push(CapabilityInput {
            name: "label".into(),
            description: "optional label".into(),
            required: false,
        });
        Ok(vec![d])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        self.availability.clone()
    }
}

fn request_input(capability: &str) -> DecisionInput {
    DecisionInput::RequestCapability {
        execution_id: ExecutionId::new("e1"),
        capability_id: capability.to_string(),
        inputs: BTreeMap::new(),
    }
}

fn agent(
    text: &'static str,
    input: DecisionInput,
    caps: Arc<Caps>,
    executor: Arc<Counting>,
) -> Agent {
    Agent::new(Arc::new(Model(text)))
        .with_decision_boundary(Arc::new(ScriptedDecision::new(input)))
        .with_capabilities(caps)
        .with_executor(executor)
}

#[tokio::test]
async fn respond_decision_creates_no_capability_request() {
    let exec = Arc::new(Counting::default());
    let a = agent(
        "hello",
        DecisionInput::Respond,
        Caps::new(CapabilityAvailability::Available),
        exec.clone(),
    );
    let report = a.decide(Turn::new("hi")).await.unwrap();
    match report.decision.unwrap() {
        AgentDecision::Respond(r) => assert_eq!(r.output, "hello"),
        other => panic!("{other:?}"),
    }
    assert_eq!(report.turn.response, "hello");
    assert_eq!(exec.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn capability_decision_is_a_semantic_request_without_execution() {
    let exec = Arc::new(Counting::default());
    let a = agent(
        "ok",
        request_input("test.op"),
        Caps::new(CapabilityAvailability::Available),
        exec.clone(),
    );
    let report = a.decide(Turn::new("hi")).await.unwrap();
    let AgentDecision::RequestCapability(request) = report.decision.unwrap() else {
        panic!()
    };
    assert_eq!(request.capability_id.as_str(), "test.op");
    assert_eq!(request.execution_id, ExecutionId::new("e1"));
    // Deciding does not execute, and the request holds no implementation detail.
    assert_eq!(exec.0.load(Ordering::SeqCst), 0);

    let execution = a.validate_capability_request(&request).await.unwrap();
    assert_eq!(
        execution,
        ExecutionRequest::new(ExecutionId::new("e1"), "test.op")
    );
    assert_eq!(
        exec.0.load(Ordering::SeqCst),
        0,
        "validation does not execute"
    );

    let report = a.execute_capability(&request).await.unwrap();
    assert!(report.result.is_ok());
    assert_eq!(exec.0.load(Ordering::SeqCst), 1);
}

async fn rejected(
    capability: &str,
    availability: CapabilityAvailability,
) -> (CapabilityError, usize) {
    let exec = Arc::new(Counting::default());
    let a = agent(
        "x",
        request_input(capability),
        Caps::new(availability),
        exec.clone(),
    );
    let report = a.decide(Turn::new("hi")).await.unwrap();
    let AgentDecision::RequestCapability(request) = report.decision.unwrap() else {
        panic!()
    };
    let err = a.execute_capability(&request).await.unwrap_err();
    (err, exec.0.load(Ordering::SeqCst))
}

#[tokio::test]
async fn unknown_capability_is_rejected_before_the_executor() {
    let (err, ran) = rejected("test.other", CapabilityAvailability::Available).await;
    assert_eq!(err, CapabilityError::Unknown("test.other".into()));
    assert_eq!(ran, 0);
}

#[tokio::test]
async fn unavailable_and_misconfigured_capabilities_cannot_reach_the_executor() {
    for state in [
        CapabilityAvailability::Unavailable("gone".into()),
        CapabilityAvailability::Misconfigured("bad".into()),
    ] {
        let (err, ran) = rejected("test.op", state).await;
        assert!(matches!(err, CapabilityError::Unavailable(_)), "{err:?}");
        assert_eq!(ran, 0);
    }
}

#[tokio::test]
async fn undeclared_or_missing_inputs_are_rejected() {
    let exec = Arc::new(Counting::default());
    let a = agent(
        "x",
        DecisionInput::Respond,
        Caps::new(CapabilityAvailability::Available),
        exec.clone(),
    );
    let id = CapabilityId::new("test.op").unwrap();

    let extra = CapabilityRequest::new(ExecutionId::new("e"), id.clone())
        .with_input("script", InputValue::Text("rm -rf /".into()));
    assert!(matches!(
        a.execute_capability(&extra).await,
        Err(CapabilityError::InvalidInput(_))
    ));

    let ok = CapabilityRequest::new(ExecutionId::new("e"), id)
        .with_input("label", InputValue::Text("fine".into()));
    assert!(a.validate_capability_request(&ok).await.is_ok());
    assert_eq!(exec.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn decision_making_only_discovers_it_never_executes() {
    let exec = Arc::new(Counting::default());
    let caps = Caps::new(CapabilityAvailability::Available);
    let a = agent("x", request_input("test.op"), caps.clone(), exec.clone());
    a.decide(Turn::new("hi")).await.unwrap();
    assert_eq!(exec.0.load(Ordering::SeqCst), 0);
    assert_eq!(caps.discoveries.load(Ordering::SeqCst), 1);
}

#[test]
fn the_boundary_can_only_return_data() {
    // `decide` is synchronous and receives only the response and descriptors:
    // there is no executor, provider or agent in its signature.
    let boundary = ScriptedDecision::new(request_input("test.op"));
    let response = ModelResponse::new("r", "text", Usage::new(0, 0));
    let decision = boundary.decide(&response, &[]).unwrap();
    assert!(matches!(decision, AgentDecision::RequestCapability(_)));
}

#[tokio::test]
async fn model_text_resembling_a_command_stays_data() {
    for text in [
        "rm -rf /",
        "$ ls -la",
        "run: python foo.py",
        "/bin/bash -c id",
    ] {
        let exec = Arc::new(Counting::default());
        let a = agent(
            text,
            DecisionInput::Respond,
            Caps::new(CapabilityAvailability::Available),
            exec.clone(),
        );
        let report = a.decide(Turn::new("hi")).await.unwrap();
        match report.decision.unwrap() {
            AgentDecision::Respond(r) => assert_eq!(r.output, text),
            other => panic!("{text:?} became {other:?}"),
        }
        assert_eq!(exec.0.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn a_command_string_is_not_a_capability_id() {
    let boundary = ScriptedDecision::new(request_input("rm -rf /"));
    let response = ModelResponse::new("r", "x", Usage::new(0, 0));
    assert_eq!(
        boundary.decide(&response, &[]),
        Err(DecisionError::Capability(CapabilityError::InvalidId(
            "rm -rf /".into()
        )))
    );
}

#[tokio::test]
async fn decision_boundary_is_optional_and_errors_are_distinct() {
    let a = Agent::new(Arc::new(Model("x")));
    let report = a.decide(Turn::new("hi")).await.unwrap();
    assert!(matches!(
        report.decision,
        Err(DecisionError::InvalidDecision(_))
    ));

    // FX failure stays an AgentError::Provider, not a decision failure.
    struct Failing;
    #[async_trait::async_trait]
    impl ModelProvider for Failing {
        async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
            Err(FxError::Provider("down".into()))
        }
    }
    let a = Agent::new(Arc::new(Failing))
        .with_decision_boundary(Arc::new(ScriptedDecision::new(DecisionInput::Respond)));
    assert!(a.decide(Turn::new("hi")).await.is_err());
}
