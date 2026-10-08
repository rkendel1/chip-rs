//! PR30: what an escalation tells the model is chosen by an `EscalationContextPolicy`, the default
//! reproduces what the loop always sent, and swapping the policy changes the request without
//! touching the loop.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, DecisionRecord, DecisionSource, EscalationContext, EscalationContextPolicy,
    ExecutionError, ExecutionId, ExecutionObserver, ExecutionRequest, ExecutionResult,
    ExecutionStatus, Executor, FullEscalationContext, ModelDecisionBoundary, NoLocalPolicy,
    Observation, ObservationKind, TestLocalReasoner, WorkDecision, WorkDecisionBoundary, WorkEvent,
    WorkGoal, WorkId, WorkSpec, WorkState, WorkTrajectory,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

// ---- the policy on its own: a golden context --------------------------------------------

#[test]
fn the_default_policy_builds_exactly_the_context_the_loop_has_always_sent() {
    let observation = Observation {
        execution_id: ExecutionId::new("e1"),
        kind: ObservationKind::ExecutionCompleted,
        status: ExecutionStatus::Success,
        output: Some("ran".into()),
        receipt_id: Some("sha256:r".into()),
    };
    let decisions = [
        DecisionRecord {
            turn: 0,
            source: DecisionSource::Local,
            decision: WorkDecision::Block { reason: "r".into() },
        },
        DecisionRecord {
            turn: 1,
            source: DecisionSource::Model,
            decision: WorkDecision::Complete {
                summary: "s".into(),
            },
        },
    ];
    let ruled_out = ["capability x.y: its execution failed".to_string()];
    let evidence = ["x.y: no valid evidence".to_string()];
    let context = FullEscalationContext.build(
        &WorkState {
            goal: "the goal",
            turn: 2,
            max_turns: 8,
            executions: 1,
            max_executions: 3,
        },
        &WorkTrajectory {
            observations: std::slice::from_ref(&observation),
            origins: &[],
            decisions: &decisions,
            ruled_out: &ruled_out,
            evidence: &evidence,
            question: "the question",
            frontier: &chip_core::DecisionFrontier::default(),
        },
    );
    assert_eq!(FullEscalationContext.id(), "full-v1");
    assert_eq!(
        context,
        EscalationContext {
            goal: "the goal".into(),
            current_state: "turn 3 of 8; executions 1 of 3".into(),
            relevant_evidence: evidence.to_vec(),
            relevant_observations: vec![observation],
            prior_decisions: vec![
                "turn 1 (local): block".into(),
                "turn 2 (model): complete".into()
            ],
            ruled_out: ruled_out.to_vec(),
            omitted: vec![],
            frontier: vec![],
            question: "the question".into(),
        }
    );
    assert_eq!(
        context.render(),
        "Goal: the goal\nState: turn 3 of 8; executions 1 of 3\nEvidence:\n  - x.y: no valid evidence\n\
Prior decisions:\n  - turn 1 (local): block\n  - turn 2 (model): complete\n\
Ruled out:\n  - capability x.y: its execution failed\nQuestion: the question"
    );
}

// ---- through the loop ---------------------------------------------------------------------

struct Replies {
    calls: AtomicUsize,
    seen: Mutex<Vec<ModelRequest>>,
}

#[async_trait::async_trait]
impl ModelProvider for Replies {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(request);
        Ok(ModelResponse::new(
            "resp-1",
            r#"{"decision":"complete","summary":"done"}"#,
            Usage::new(7, 3),
        ))
    }
}

struct Exec(AtomicUsize);

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ExecutionResult::success(r.id, "ran").with_receipt_id("sha256:real"))
    }
}

struct Caps;

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(vec![CapabilityDescriptor::new(
            CapabilityId::new("compute.selftest")?,
            "t",
            "t",
        )])
    }
    async fn availability(&self, _: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

fn setup() -> (Arc<Replies>, Arc<Exec>, Agent) {
    let model = Arc::new(Replies {
        calls: AtomicUsize::new(0),
        seen: Mutex::new(vec![]),
    });
    let exec = Arc::new(Exec(AtomicUsize::new(0)));
    let agent = Agent::new(model.clone())
        .with_capabilities(Arc::new(Caps))
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()));
    (model, exec, agent)
}

fn spec() -> WorkSpec {
    WorkSpec::new(
        WorkId::new("w"),
        WorkGoal::new("Decide about compute.selftest"),
    )
}

/// A deliberately tiny policy: the goal and the question, nothing else.
struct OnlyGoalContextPolicy;

impl EscalationContextPolicy for OnlyGoalContextPolicy {
    fn id(&self) -> &'static str {
        "only-goal-test"
    }
    fn build(&self, state: &WorkState<'_>, trajectory: &WorkTrajectory<'_>) -> EscalationContext {
        EscalationContext {
            goal: state.goal.to_string(),
            current_state: String::new(),
            relevant_evidence: vec![],
            relevant_observations: vec![],
            prior_decisions: vec![],
            ruled_out: vec![],
            omitted: vec![],
            frontier: vec![],
            question: trajectory.question.to_string(),
        }
    }
}

#[tokio::test]
async fn an_injected_policy_decides_what_the_model_is_sent() {
    let (model, _, agent) = setup();
    let report = agent
        .run_work_with_context_policy(
            &spec(),
            &NoLocalPolicy,
            &ModelDecisionBoundary,
            &OnlyGoalContextPolicy,
        )
        .await;

    let sent = model.seen.lock().unwrap()[0].clone();
    assert_eq!(sent.messages.len(), 1);
    let caps = agent.discover_capabilities().await.result.unwrap();
    let question = ModelDecisionBoundary.question(&caps);
    assert_eq!(
        sent.messages[0].content,
        format!(
            "Goal: Decide about compute.selftest\nState: \nEvidence:\nPrior decisions:\nRuled out:\nQuestion: {question}"
        )
    );

    // The measurement is of what was sent, and names the policy that chose it.
    let m = report.measurement();
    assert_eq!(m.context_policy.as_deref(), Some("only-goal-test"));
    let bytes: usize = sent.messages.iter().map(|x| x.content.len()).sum();
    assert_eq!(m.context_bytes as usize, bytes);
    assert!(matches!(
        report.events.iter().find_map(|e| match e {
            WorkEvent::ModelEscalation { context_policy, .. } => Some(context_policy.clone()),
            _ => None,
        }),
        Some(p) if p == "only-goal-test"
    ));
}

#[tokio::test]
async fn run_work_uses_the_full_policy_and_says_so() {
    let (model, _, agent) = setup();
    let default = agent
        .run_work(&spec(), &NoLocalPolicy, &ModelDecisionBoundary)
        .await;
    let (model2, _, agent2) = setup();
    let explicit = agent2
        .run_work_with_context_policy(
            &spec(),
            &NoLocalPolicy,
            &ModelDecisionBoundary,
            &FullEscalationContext,
        )
        .await;

    // Same provider request, same trajectory, same outcome.
    assert_eq!(
        model.seen.lock().unwrap()[0].messages,
        model2.seen.lock().unwrap()[0].messages
    );
    assert_eq!(default.events, explicit.events);
    assert_eq!(default.outcome, explicit.outcome);

    let m = default.measurement();
    assert_eq!(m.context_policy.as_deref(), Some("full-v1"));
    let sent = model.seen.lock().unwrap()[0].clone();
    let bytes: usize = sent.messages.iter().map(|x| x.content.len()).sum();
    assert_eq!(m.context_bytes as usize, bytes);
}

#[tokio::test]
async fn a_run_that_never_escalates_names_no_policy() {
    let (model, _, agent) = setup();
    let report = agent
        .run_work(
            &spec(),
            &chip_core::ScriptedPolicy::new(vec![Some(WorkDecision::Complete {
                summary: "x".into(),
            })]),
            &ModelDecisionBoundary,
        )
        .await;
    assert_eq!(report.measurement().context_policy, None);
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

// ---- isolation ---------------------------------------------------------------------------

#[test]
fn the_policy_is_data_in_data_out() {
    let src =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/work.rs")).unwrap();
    let start = src.find("pub struct WorkState").unwrap();
    let end = src.find("/// The ordered trajectory.").unwrap();
    let policy = &src[start..end];
    for forbidden in [
        "Executor",
        "ModelProvider",
        "Agent",
        "async",
        "fx_provider",
        "chip_compute",
        "AppPort",
        "Attn",
        "std::env",
        "std::fs",
    ] {
        assert!(
            !policy.contains(forbidden),
            "policy code mentions {forbidden}"
        );
    }
}
