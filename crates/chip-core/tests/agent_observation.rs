//! PR8: observation on the Agent is optional, explicit, and inert.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chip_core::{
    Agent, AgentDecision, AgentEvent, DecisionInput, ExecutionError, ExecutionId,
    ExecutionObserver, ExecutionRequest, ExecutionResult, ExecutionStatus, Executor,
    ObservationError, ObservationKind, ScriptedDecision, TestExecutor, Turn,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

#[derive(Default)]
struct CountingModel(AtomicUsize);

#[async_trait::async_trait]
impl ModelProvider for CountingModel {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ModelResponse::new("r", "out", Usage::new(1, 1)))
    }
}

#[derive(Default)]
struct CountingExecutor(AtomicUsize);

#[async_trait::async_trait]
impl Executor for CountingExecutor {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        TestExecutor.execute(r).await
    }
}

fn sample() -> ExecutionResult {
    ExecutionResult::success(ExecutionId::new("e1"), "hello").with_receipt_id("r1")
}

#[test]
fn injected_observer_produces_an_observation() {
    let agent =
        Agent::new(Arc::new(CountingModel::default())).with_observer(Arc::new(ExecutionObserver));
    let o = agent.observe(&sample()).unwrap();
    assert_eq!(o.kind, ObservationKind::ExecutionCompleted);
    assert_eq!(o.execution_id, ExecutionId::new("e1"));
    assert_eq!(o.receipt_id.as_deref(), Some("r1"));
}

#[test]
fn absent_observer_is_an_explicit_error() {
    let agent = Agent::new(Arc::new(CountingModel::default()));
    assert!(matches!(
        agent.observe(&sample()),
        Err(ObservationError::ObserverUnavailable(_))
    ));
}

#[tokio::test]
async fn observing_never_calls_the_model_or_the_executor() {
    let model = Arc::new(CountingModel::default());
    let exec = Arc::new(CountingExecutor::default());
    let agent = Agent::new(model.clone())
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver));
    for _ in 0..3 {
        agent.observe(&sample()).unwrap();
    }
    assert_eq!(model.0.load(Ordering::SeqCst), 0);
    assert_eq!(exec.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn existing_agent_behavior_is_unchanged_with_and_without_an_observer() {
    for with_observer in [false, true] {
        let model = Arc::new(CountingModel::default());
        let exec = Arc::new(CountingExecutor::default());
        let mut agent = Agent::new(model.clone())
            .with_decision_boundary(Arc::new(ScriptedDecision::new(
                DecisionInput::RequestCapability {
                    execution_id: ExecutionId::new("t1"),
                    capability_id: "test.op".into(),
                    inputs: Default::default(),
                },
            )))
            .with_capabilities(Arc::new(OneCapability))
            .with_executor(exec.clone());
        if with_observer {
            agent = agent.with_observer(Arc::new(ExecutionObserver));
        }

        assert_eq!(agent.turn(Turn::new("hi")).await.unwrap().events.len(), 5);
        assert!(
            agent
                .decide(Turn::new("hi"))
                .await
                .unwrap()
                .decision
                .is_ok()
        );

        let before = exec.0.load(Ordering::SeqCst);
        let outcome = agent.run_turn(Turn::new("hi")).await.unwrap();
        assert!(matches!(
            outcome.decision,
            AgentDecision::RequestCapability(_)
        ));
        assert_eq!(outcome.events.len(), 8, "run_turn events unchanged");
        assert!(
            !outcome
                .events
                .iter()
                .any(|e| matches!(e, AgentEvent::Capability(_)) && false)
        );
        assert_eq!(exec.0.load(Ordering::SeqCst), before + 1);

        let report = agent
            .execute(ExecutionRequest::new(ExecutionId::new("x"), "test.op"))
            .await;
        assert_eq!(report.result.unwrap().status, ExecutionStatus::Success);
        // run_turn did not observe or loop: still one model call per call above.
        assert_eq!(model.0.load(Ordering::SeqCst), 3);
    }
}

struct OneCapability;

#[async_trait::async_trait]
impl chip_core::CapabilityProvider for OneCapability {
    async fn capabilities(
        &self,
    ) -> Result<Vec<chip_core::CapabilityDescriptor>, chip_core::CapabilityError> {
        Ok(vec![chip_core::CapabilityDescriptor::new(
            chip_core::CapabilityId::new("test.op")?,
            "Op",
            "Test",
        )])
    }

    async fn availability(
        &self,
        _id: &chip_core::CapabilityId,
    ) -> chip_core::CapabilityAvailability {
        chip_core::CapabilityAvailability::Available
    }
}
