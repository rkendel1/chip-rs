//! PR7: one bounded turn: one model call, one decision, zero or one execution.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chip_core::{
    Agent, AgentDecision, AgentError, AgentEvent, CapabilityAvailability, CapabilityDescriptor,
    CapabilityError, CapabilityEvent, CapabilityId, CapabilityInput, CapabilityProvider,
    DecisionBoundary, DecisionError, DecisionInput, ExecutionError, ExecutionEvent, ExecutionId,
    ExecutionRequest, ExecutionResult, ExecutionStatus, Executor, InputValue, ScriptedDecision,
    TestExecutor, Turn,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

#[derive(Default)]
struct Model {
    calls: AtomicUsize,
    fail: bool,
}

#[async_trait::async_trait]
impl ModelProvider for Model {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(FxError::Provider("down".into()));
        }
        Ok(ModelResponse::new("r1", "model output", Usage::new(1, 1)))
    }
}

#[derive(Default)]
struct Exec {
    calls: AtomicUsize,
    fail_result: bool,
}

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_result {
            return Ok(ExecutionResult::failure(r.id, "operation failed"));
        }
        TestExecutor.execute(r).await
    }
}

struct Caps(CapabilityAvailability);

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut d = CapabilityDescriptor::new(CapabilityId::new("test.op").unwrap(), "Op", "Test");
        d.inputs.push(CapabilityInput {
            name: "label".into(),
            description: "optional".into(),
            required: false,
        });
        Ok(vec![d])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        self.0.clone()
    }
}

struct CountingDecision {
    calls: AtomicUsize,
    inner: ScriptedDecision,
}

impl DecisionBoundary for CountingDecision {
    fn decide(
        &self,
        response: &ModelResponse,
        capabilities: &[chip_core::Capability],
    ) -> Result<AgentDecision, DecisionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.decide(response, capabilities)
    }
}

fn request(capability: &str, inputs: BTreeMap<String, InputValue>) -> DecisionInput {
    DecisionInput::RequestCapability {
        execution_id: ExecutionId::new("exec-7"),
        capability_id: capability.into(),
        inputs,
    }
}

struct Rig {
    agent: Agent,
    model: Arc<Model>,
    exec: Arc<Exec>,
    decision: Arc<CountingDecision>,
}

fn rig(input: DecisionInput, availability: CapabilityAvailability) -> Rig {
    rig_with(input, availability, Model::default(), Exec::default())
}

fn rig_with(
    input: DecisionInput,
    availability: CapabilityAvailability,
    model: Model,
    exec: Exec,
) -> Rig {
    let model = Arc::new(model);
    let exec = Arc::new(exec);
    let decision = Arc::new(CountingDecision {
        calls: AtomicUsize::new(0),
        inner: ScriptedDecision::new(input),
    });
    let agent = Agent::new(model.clone())
        .with_decision_boundary(decision.clone())
        .with_capabilities(Arc::new(Caps(availability)))
        .with_executor(exec.clone());
    Rig {
        agent,
        model,
        exec,
        decision,
    }
}

#[tokio::test]
async fn response_only_turn_has_no_execution() {
    let r = rig(DecisionInput::Respond, CapabilityAvailability::Available);
    let outcome = r.agent.run_turn(Turn::new("hi")).await.unwrap();
    assert_eq!(outcome.turn, Turn::new("hi"));
    assert_eq!(outcome.response.output, "model output");
    assert!(matches!(outcome.decision, AgentDecision::Respond(_)));
    assert_eq!(outcome.execution, None);
    assert_eq!(r.exec.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        outcome.events,
        vec![
            AgentEvent::TurnStarted {
                message: "hi".into()
            },
            AgentEvent::Capability(CapabilityEvent::CapabilitiesRequested),
            AgentEvent::Capability(CapabilityEvent::CapabilitiesAvailable { count: 1 }),
            AgentEvent::DecisionMade { capability: None },
            AgentEvent::TurnCompleted {
                response: "model output".into()
            },
        ]
    );
}

#[tokio::test]
async fn capability_turn_executes_once_with_a_consistent_id_chain() {
    let r = rig(
        request("test.op", BTreeMap::new()),
        CapabilityAvailability::Available,
    );
    let outcome = r.agent.run_turn(Turn::new("hi")).await.unwrap();

    let AgentDecision::RequestCapability(cap) = &outcome.decision else {
        panic!()
    };
    let result = outcome.execution.clone().unwrap();
    assert_eq!(result.status, ExecutionStatus::Success);
    // CapabilityRequest.execution_id == ExecutionRequest.id == ExecutionResult.id
    assert_eq!(cap.execution_id, ExecutionId::new("exec-7"));
    assert_eq!(result.id, cap.execution_id);
    assert_eq!(r.exec.calls.load(Ordering::SeqCst), 1);

    let id = ExecutionId::new("exec-7");
    assert_eq!(
        outcome.events,
        vec![
            AgentEvent::TurnStarted {
                message: "hi".into()
            },
            AgentEvent::Capability(CapabilityEvent::CapabilitiesRequested),
            AgentEvent::Capability(CapabilityEvent::CapabilitiesAvailable { count: 1 }),
            AgentEvent::DecisionMade {
                capability: Some(CapabilityId::new("test.op").unwrap())
            },
            AgentEvent::Execution(ExecutionEvent::ExecutionRequested {
                id: id.clone(),
                intent: "test.op".into()
            }),
            AgentEvent::Execution(ExecutionEvent::ExecutionStarted { id: id.clone() }),
            AgentEvent::Execution(ExecutionEvent::ExecutionCompleted {
                id,
                output: "test execution completed".into()
            }),
            AgentEvent::TurnCompleted {
                response: "model output".into()
            },
        ]
    );
}

async fn rejected(input: DecisionInput, availability: CapabilityAvailability) -> (AgentError, Rig) {
    let r = rig(input, availability);
    let err = r.agent.run_turn(Turn::new("hi")).await.unwrap_err();
    assert_eq!(
        r.exec.calls.load(Ordering::SeqCst),
        0,
        "executor must not run"
    );
    (err, r)
}

#[tokio::test]
async fn unknown_capability_fails_validation_without_execution() {
    let (err, _) = rejected(
        request("test.other", BTreeMap::new()),
        CapabilityAvailability::Available,
    )
    .await;
    assert_eq!(
        err,
        AgentError::Capability(CapabilityError::Unknown("test.other".into()))
    );
}

#[tokio::test]
async fn unavailable_and_misconfigured_fail_validation_without_execution() {
    for state in [
        CapabilityAvailability::Unavailable("gone".into()),
        CapabilityAvailability::Misconfigured("bad".into()),
    ] {
        let (err, _) = rejected(request("test.op", BTreeMap::new()), state).await;
        assert!(
            matches!(err, AgentError::Capability(CapabilityError::Unavailable(_))),
            "{err:?}"
        );
    }
}

#[tokio::test]
async fn invalid_input_fails_validation_without_execution() {
    let mut inputs = BTreeMap::new();
    inputs.insert("script".to_string(), InputValue::Text("rm -rf /".into()));
    let (err, _) = rejected(
        request("test.op", inputs),
        CapabilityAvailability::Available,
    )
    .await;
    assert!(
        matches!(
            err,
            AgentError::Capability(CapabilityError::InvalidInput(_))
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn execution_failure_is_an_outcome_not_an_error() {
    let r = rig_with(
        request("test.op", BTreeMap::new()),
        CapabilityAvailability::Available,
        Model::default(),
        Exec {
            fail_result: true,
            ..Default::default()
        },
    );
    let outcome = r.agent.run_turn(Turn::new("hi")).await.unwrap();
    assert!(matches!(
        outcome.decision,
        AgentDecision::RequestCapability(_)
    ));
    let result = outcome.execution.unwrap();
    assert_eq!(result.status, ExecutionStatus::Failure);
    assert_eq!(result.output, "operation failed");
    assert!(outcome.events.iter().any(|e| matches!(
        e,
        AgentEvent::Execution(ExecutionEvent::ExecutionFailed { .. })
    )));
}

#[tokio::test]
async fn executor_unable_to_run_is_an_execution_error() {
    struct Down;
    #[async_trait::async_trait]
    impl Executor for Down {
        async fn execute(&self, _r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
            Err(ExecutionError::ExecutorUnavailable("offline".into()))
        }
    }
    let r = rig(
        request("test.op", BTreeMap::new()),
        CapabilityAvailability::Available,
    );
    let agent = r.agent.with_executor(Arc::new(Down));
    let err = agent.run_turn(Turn::new("hi")).await.unwrap_err();
    assert!(matches!(
        err,
        AgentError::Execution(ExecutionError::ExecutorUnavailable(_))
    ));
}

#[tokio::test]
async fn provider_failure_stops_before_decision_and_execution() {
    let r = rig_with(
        request("test.op", BTreeMap::new()),
        CapabilityAvailability::Available,
        Model {
            fail: true,
            ..Default::default()
        },
        Exec::default(),
    );
    let err = r.agent.run_turn(Turn::new("hi")).await.unwrap_err();
    assert!(matches!(err, AgentError::Provider(_)));
    assert_eq!(r.decision.calls.load(Ordering::SeqCst), 0);
    assert_eq!(r.exec.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn missing_decision_boundary_is_a_decision_error_without_execution() {
    let exec = Arc::new(Exec::default());
    let agent = Agent::new(Arc::new(Model::default())).with_executor(exec.clone());
    let err = agent.run_turn(Turn::new("hi")).await.unwrap_err();
    assert!(matches!(
        err,
        AgentError::Decision(DecisionError::InvalidDecision(_))
    ));
    assert_eq!(exec.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn one_turn_never_continues_on_its_own() {
    for input in [DecisionInput::Respond, request("test.op", BTreeMap::new())] {
        let expected_exec = usize::from(!matches!(input, DecisionInput::Respond));
        let r = rig(input, CapabilityAvailability::Available);
        r.agent.run_turn(Turn::new("hi")).await.unwrap();
        assert_eq!(
            r.model.calls.load(Ordering::SeqCst),
            1,
            "exactly one model call"
        );
        assert_eq!(
            r.decision.calls.load(Ordering::SeqCst),
            1,
            "exactly one decision"
        );
        assert_eq!(r.exec.calls.load(Ordering::SeqCst), expected_exec);
    }
}

#[tokio::test]
async fn existing_turn_api_is_unchanged() {
    let r = rig(DecisionInput::Respond, CapabilityAvailability::Available);
    let result = r.agent.turn(Turn::new("hi")).await.unwrap();
    assert_eq!(result.events.len(), 5);
}
