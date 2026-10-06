//! PR10: bounded workload proof (deterministic). The caller drives every step:
//! Turn 1 -> capability request -> execute -> observe -> Turn 2 -> stop.
//! The same flow runs against real Compute in the chip-compute tests and the CLI.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, AgentDecision, Capability, CapabilityAvailability, CapabilityDescriptor,
    CapabilityError, CapabilityId, CapabilityProvider, CapabilityRequest, DecisionBoundary,
    DecisionError, DecisionInput, ExecutionError, ExecutionId, ExecutionObserver, ExecutionRequest,
    ExecutionResult, ExecutionStatus, Executor, Observation, ObservationKind, ScriptedDecision,
    Turn,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

const CAPABILITY: &str = "compute.selftest";

#[derive(Default)]
struct Model(Mutex<Vec<ModelRequest>>);

#[async_trait::async_trait]
impl ModelProvider for Model {
    async fn complete(&self, r: ModelRequest) -> Result<ModelResponse, FxError> {
        let seen_failure = r
            .messages
            .iter()
            .any(|m| m.content.contains("kind: execution.failed"));
        let seen_completion = r
            .messages
            .iter()
            .any(|m| m.content.contains("kind: execution.completed"));
        self.0.lock().unwrap().push(r);
        let text = match (seen_completion, seen_failure) {
            (true, _) => "observed the execution result: completed",
            (_, true) => "observed the execution result: failed",
            _ => "run the self test",
        };
        Ok(ModelResponse::new("r", text, Usage::new(1, 1)))
    }
}

/// Executor double: counts calls and returns a fixed result for the request id.
struct Exec {
    calls: AtomicUsize,
    status: ExecutionStatus,
    output: &'static str,
}

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ExecutionResult {
            id: r.id,
            status: self.status,
            output: self.output.to_string(),
            receipt_id: Some("sha256:fixed-receipt".into()),
        })
    }
}

/// Decision 1 requests the capability; decision 2 follows `second`.
struct Decisions {
    calls: AtomicUsize,
    second_requests_again: bool,
}

impl DecisionBoundary for Decisions {
    fn decide(
        &self,
        response: &ModelResponse,
        caps: &[Capability],
    ) -> Result<AgentDecision, DecisionError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let input = if n == 0 || self.second_requests_again {
            DecisionInput::RequestCapability {
                execution_id: ExecutionId::new(format!("proof-{}", n + 1)),
                capability_id: CAPABILITY.into(),
                inputs: Default::default(),
            }
        } else {
            DecisionInput::Respond
        };
        ScriptedDecision::new(input).decide(response, caps)
    }
}

struct Caps;

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(vec![CapabilityDescriptor::new(
            CapabilityId::new(CAPABILITY)?,
            "Self Test",
            "Deterministic",
        )])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

struct Run {
    model: Arc<Model>,
    exec: Arc<Exec>,
    decisions: Arc<Decisions>,
    capability_request: CapabilityRequest,
    execution: ExecutionResult,
    observation: Observation,
    turn2: AgentDecision,
}

async fn cycle(status: ExecutionStatus, output: &'static str, second_requests_again: bool) -> Run {
    let model = Arc::new(Model::default());
    let exec = Arc::new(Exec {
        calls: AtomicUsize::new(0),
        status,
        output,
    });
    let decisions = Arc::new(Decisions {
        calls: AtomicUsize::new(0),
        second_requests_again,
    });
    let agent = Agent::new(model.clone())
        .with_capabilities(Arc::new(Caps))
        .with_decision_boundary(decisions.clone())
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver));

    // Turn 1: decide only. Nothing has executed.
    let first = agent
        .decide(Turn::new(
            "Perform a bounded self-test and report what happened",
        ))
        .await
        .unwrap();
    let AgentDecision::RequestCapability(capability_request) = first.decision.unwrap() else {
        panic!()
    };
    assert_eq!(exec.calls.load(Ordering::SeqCst), 0);

    // Explicit execution, explicit observation.
    let execution = agent
        .execute_capability(&capability_request)
        .await
        .unwrap()
        .result
        .unwrap();
    let observation = agent.observe(&execution).unwrap();

    // Explicit Turn 2 with the observation.
    let second = agent
        .decide_with_observations(
            Turn::new("What actually happened?"),
            std::slice::from_ref(&observation),
        )
        .await
        .unwrap();
    Run {
        model,
        exec,
        decisions,
        capability_request,
        execution,
        observation,
        turn2: second.decision.unwrap(),
    }
}

#[tokio::test]
async fn successful_cycle_is_bounded_and_the_id_chain_holds() {
    let run = cycle(ExecutionStatus::Success, "selftest ok", false).await;

    // Boundedness: 2 model calls, 1 capability request, 1 execution, 1 observation.
    assert_eq!(run.model.0.lock().unwrap().len(), 2);
    assert_eq!(run.decisions.calls.load(Ordering::SeqCst), 2);
    assert_eq!(run.exec.calls.load(Ordering::SeqCst), 1);
    assert_eq!(run.capability_request.capability_id.as_str(), CAPABILITY);
    assert!(matches!(run.turn2, AgentDecision::Respond(ref r) if r.output.contains("completed")));

    // Id chain: request -> result -> observation -> the Turn 2 model request.
    let id = ExecutionId::new("proof-1");
    assert_eq!(run.capability_request.execution_id, id);
    assert_eq!(run.execution.id, id);
    assert_eq!(run.observation.execution_id, id);
    let requests = run.model.0.lock().unwrap();
    assert_eq!(requests[0].messages.len(), 1, "Turn 1 saw no observation");
    assert_eq!(requests[1].messages[0].content, run.observation.render());
    assert!(
        requests[1].messages[0]
            .content
            .contains("execution_id: \"proof-1\"")
    );
}

#[tokio::test]
async fn failed_execution_reaches_turn_two_as_failed_reality() {
    let run = cycle(
        ExecutionStatus::Failure,
        "exit code 3: selftest crashed",
        false,
    )
    .await;
    assert_eq!(run.observation.kind, ObservationKind::ExecutionFailed);
    assert_eq!(run.observation.status, ExecutionStatus::Failure);
    let sent = run.model.0.lock().unwrap()[1].messages[0].content.clone();
    assert!(sent.contains("kind: execution.failed") && sent.contains("status: failure"));
    assert!(sent.contains("selftest crashed"));
    assert!(matches!(run.turn2, AgentDecision::Respond(ref r) if r.output.contains("failed")));
    // No automatic retry.
    assert_eq!(run.exec.calls.load(Ordering::SeqCst), 1);
    assert_eq!(run.model.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn reality_is_preserved_and_output_cannot_override_status() {
    // Compute-style result: successful status, output that *says* failure.
    let run = cycle(ExecutionStatus::Success, "selftest failed", false).await;
    assert_eq!(run.observation.kind, ObservationKind::ExecutionCompleted);
    assert_eq!(run.observation.status, ExecutionStatus::Success);
    assert_eq!(run.observation.output.as_deref(), Some("selftest failed"));
    assert_eq!(
        run.observation.receipt_id.as_deref(),
        Some("sha256:fixed-receipt")
    );
    assert_eq!(run.observation.execution_id, run.execution.id);

    // And the reverse: failure status with reassuring output stays a failure.
    let run = cycle(ExecutionStatus::Failure, "all tests passed", false).await;
    assert_eq!(run.observation.kind, ObservationKind::ExecutionFailed);
    assert_eq!(run.observation.output.as_deref(), Some("all tests passed"));
}

#[tokio::test]
async fn turn_two_never_executes_even_if_it_requests_another_capability() {
    let run = cycle(ExecutionStatus::Success, "ok", true).await;
    let AgentDecision::RequestCapability(again) = &run.turn2 else {
        panic!("expected a request")
    };
    assert_eq!(again.execution_id, ExecutionId::new("proof-2"));
    assert_eq!(
        run.exec.calls.load(Ordering::SeqCst),
        1,
        "returned to the caller, not executed"
    );
    assert_eq!(
        run.model.0.lock().unwrap().len(),
        2,
        "no automatic third turn"
    );
}
