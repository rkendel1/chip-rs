//! PR9: the first complete bounded cycle, driven explicitly by the caller.
//! Turn 1 -> capability -> execution -> observation -> Turn 2 -> stop.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, AgentDecision, Capability, CapabilityAvailability, CapabilityDescriptor,
    CapabilityError, CapabilityId, CapabilityProvider, DecisionBoundary, DecisionError,
    DecisionInput, ExecutionError, ExecutionId, ExecutionObserver, ExecutionRequest,
    ExecutionResult, ExecutionStatus, Executor, Observation, ObservationKind, Observer,
    ScriptedDecision, TestExecutor, Turn,
};
use fx_core::{FxError, MessageRole, ModelProvider, ModelRequest, ModelResponse, Usage};

#[derive(Default)]
struct Model(Mutex<Vec<ModelRequest>>);

#[async_trait::async_trait]
impl ModelProvider for Model {
    async fn complete(&self, r: ModelRequest) -> Result<ModelResponse, FxError> {
        let n = {
            let mut requests = self.0.lock().unwrap();
            requests.push(r);
            requests.len()
        };
        Ok(ModelResponse::new(
            format!("r{n}"),
            format!("reply {n}"),
            Usage::new(1, 1),
        ))
    }
}

#[derive(Default)]
struct Exec(AtomicUsize);

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(TestExecutor
            .execute(r)
            .await?
            .with_receipt_id("sha256:cycle"))
    }
}

/// First decision requests a capability; the second responds.
struct Sequence(AtomicUsize);

impl DecisionBoundary for Sequence {
    fn decide(
        &self,
        response: &ModelResponse,
        caps: &[Capability],
    ) -> Result<AgentDecision, DecisionError> {
        let input = match self.0.fetch_add(1, Ordering::SeqCst) {
            0 => DecisionInput::RequestCapability {
                execution_id: ExecutionId::new("cycle-1"),
                capability_id: "compute.selftest".into(),
                inputs: Default::default(),
            },
            _ => DecisionInput::Respond,
        };
        ScriptedDecision::new(input).decide(response, caps)
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

#[tokio::test]
async fn execute_once_observe_reason_once_stop() {
    let model = Arc::new(Model::default());
    let exec = Arc::new(Exec::default());
    let decisions = Arc::new(Sequence(AtomicUsize::new(0)));
    let agent = Agent::new(model.clone())
        .with_capabilities(Arc::new(Caps))
        .with_decision_boundary(decisions.clone())
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver));

    // Turn 1: the model's decision requests a capability. Nothing runs yet.
    let first = agent.decide(Turn::new("run the self test")).await.unwrap();
    let AgentDecision::RequestCapability(request) = first.decision.unwrap() else {
        panic!()
    };
    assert_eq!(exec.0.load(Ordering::SeqCst), 0);

    // Explicit execution.
    let report = agent.execute_capability(&request).await.unwrap();
    let result = report.result.unwrap();
    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(exec.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        model.0.lock().unwrap().len(),
        1,
        "executing does not call the model"
    );

    // Explicit observation.
    let observation: Observation = agent.observe(&result).unwrap();
    assert_eq!(observation.kind, ObservationKind::ExecutionCompleted);
    assert_eq!(observation.execution_id, ExecutionId::new("cycle-1"));
    assert_eq!(
        model.0.lock().unwrap().len(),
        1,
        "observing does not call the model"
    );

    // Explicit Turn 2 with the observation.
    let second = agent
        .decide_with_observations(
            Turn::new("what happened?"),
            std::slice::from_ref(&observation),
        )
        .await
        .unwrap();
    assert!(matches!(second.decision, Ok(AgentDecision::Respond(_))));

    // Exactly two model calls, one execution, one observation; nothing further.
    let requests = model.0.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(exec.0.load(Ordering::SeqCst), 1);
    assert_eq!(decisions.0.load(Ordering::SeqCst), 2);

    // Turn 1 saw no observation; Turn 2 saw exactly the one.
    assert_eq!(requests[0].messages.len(), 1);
    assert_eq!(requests[1].messages.len(), 2);
    assert_eq!(requests[1].messages[0].role, MessageRole::System);
    assert_eq!(requests[1].messages[0].content, observation.render());
    assert!(requests[1].messages[0].content.contains("cycle-1"));
    assert!(requests[1].messages[0].content.contains("sha256:cycle"));
}

#[tokio::test]
async fn a_second_capability_request_is_left_to_the_caller() {
    struct Always;
    impl DecisionBoundary for Always {
        fn decide(
            &self,
            response: &ModelResponse,
            caps: &[Capability],
        ) -> Result<AgentDecision, DecisionError> {
            ScriptedDecision::new(DecisionInput::RequestCapability {
                execution_id: ExecutionId::new("again"),
                capability_id: "compute.selftest".into(),
                inputs: Default::default(),
            })
            .decide(response, caps)
        }
    }
    let model = Arc::new(Model::default());
    let exec = Arc::new(Exec::default());
    let agent = Agent::new(model.clone())
        .with_capabilities(Arc::new(Caps))
        .with_decision_boundary(Arc::new(Always))
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver));

    let result = ExecutionResult::success(ExecutionId::new("cycle-1"), "ok");
    let observation = agent.observe(&result).unwrap();
    let report = agent
        .decide_with_observations(Turn::new("next?"), &[observation])
        .await
        .unwrap();
    assert!(matches!(
        report.decision,
        Ok(AgentDecision::RequestCapability(_))
    ));
    assert_eq!(
        exec.0.load(Ordering::SeqCst),
        0,
        "cycle stops; nothing auto-executed"
    );
    assert_eq!(model.0.lock().unwrap().len(), 1);
}

#[test]
fn cancelled_result_and_cancelled_error_remain_distinct() {
    // A cancelled *result* is observable; an executor that produced no result
    // reports ExecutionError::Cancelled, which is not turned into a result.
    let result = ExecutionResult {
        id: ExecutionId::new("c"),
        status: ExecutionStatus::Cancelled,
        output: String::new(),
        receipt_id: None,
    };
    assert_eq!(
        ExecutionObserver.observe(&result).unwrap().kind,
        ObservationKind::ExecutionCancelled
    );
    let error: Result<ExecutionResult, ExecutionError> = Err(ExecutionError::Cancelled);
    assert!(matches!(error, Err(ExecutionError::Cancelled)));
}
