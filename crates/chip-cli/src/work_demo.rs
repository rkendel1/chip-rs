//! `chip-cli --test-work [scenario]` and `--test-real-work`: the bounded autonomous loop, end to end.
//!
//! One `run_work` call makes every decision; this module never starts a second turn. The model is
//! a deterministic stand-in, so nothing here needs credentials. `--test-real-work` uses real
//! Compute and exits with status 3 (skipped) when Compute is not available.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chip_core::{
    Agent, Capability, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, CapabilityRequest, DecisionError, EvidenceState, ExecutionError,
    ExecutionId, ExecutionObserver, ExecutionRequest, ExecutionResult, ExecutionStatus, Executor,
    LimitKind, LocalReasoningResult, LocalWorkPolicy, ObservationKind, ScriptedPolicy,
    TerminalState, TestLocalReasoner, WorkDecision, WorkDecisionBoundary, WorkEvent, WorkGoal,
    WorkId, WorkLimits, WorkOutcome, WorkReport, WorkSpec, WorkView,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

const CAPABILITY: &str = "compute.selftest";

/// Counts calls; answers every escalation with the same text.
struct StandInModel(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl ModelProvider for StandInModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, FxError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ModelResponse::new(
            "work-model",
            "run the self test",
            Usage::new(1, 1),
        ))
    }
}

/// Plays back a fixed list of decisions, one per escalation.
struct ModelDecisions(Vec<WorkDecision>, AtomicUsize);

impl WorkDecisionBoundary for ModelDecisions {
    fn interpret(
        &self,
        _r: &ModelResponse,
        _c: &[Capability],
    ) -> Result<WorkDecision, DecisionError> {
        let i = self.1.fetch_add(1, Ordering::SeqCst);
        self.0
            .get(i)
            .cloned()
            .ok_or_else(|| DecisionError::InvalidDecision("no more scripted decisions".into()))
    }
}

struct CountingExecutor(Arc<dyn Executor>, Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Executor for CountingExecutor {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.execute(request).await
    }
}

/// Deterministic executor: a fixed status and a fixed receipt.
struct Fixed(ExecutionStatus);

#[async_trait::async_trait]
impl Executor for Fixed {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        Ok(ExecutionResult {
            id: request.id,
            status: self.0,
            output: match self.0 {
                ExecutionStatus::Success => "self test passed".into(),
                _ => "self test failed".into(),
            },
            receipt_id: Some("sha256:demo-receipt".into()),
        })
    }
}

struct SelfTestCapability;

#[async_trait::async_trait]
impl CapabilityProvider for SelfTestCapability {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut descriptor = CapabilityDescriptor::new(
            CapabilityId::new(CAPABILITY)?,
            "Self test",
            "Deterministic self test",
        );
        descriptor.inputs.push(chip_core::CapabilityInput {
            name: "n".into(),
            description: "variant".into(),
            required: false,
        });
        Ok(vec![descriptor])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

fn request(execution: &str) -> CapabilityRequest {
    CapabilityRequest::new(
        ExecutionId::new(execution),
        CapabilityId::new(CAPABILITY).unwrap(),
    )
}

/// Decides from what has been observed: request the self test; complete if it passed, block if it
/// failed. The decisions are the policy's reaction to the observation, not a fabricated outcome.
struct ReactToObservation;

impl LocalWorkPolicy for ReactToObservation {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        match view.observations.last() {
            None => Some(WorkDecision::RequestCapability(request("work-1"))),
            Some(o) if o.kind == ObservationKind::ExecutionCompleted => {
                Some(WorkDecision::Complete {
                    summary: format!(
                        "the self test passed (receipt {})",
                        o.receipt_id.as_deref().unwrap_or("none")
                    ),
                })
            }
            Some(o) if o.kind == ObservationKind::ExecutionFailed => Some(WorkDecision::Block {
                reason: "the self test failed".into(),
            }),
            Some(_) => Some(WorkDecision::Escalate {
                reason: "the execution was cancelled".into(),
            }),
        }
    }
}

/// Always asks for another distinct execution (a different input each time).
struct Insatiable;

impl LocalWorkPolicy for Insatiable {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        Some(WorkDecision::RequestCapability(
            request(&format!("work-{}", view.turn))
                .with_input("n", chip_core::InputValue::Integer(view.turn as i64)),
        ))
    }
}

/// Always asks for the very same thing, so evidence answers every time after the first.
struct Repeating;

impl LocalWorkPolicy for Repeating {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        Some(WorkDecision::RequestCapability(request(&format!(
            "work-{}",
            view.turn
        ))))
    }
}

fn describe(event: &WorkEvent) -> String {
    match event {
        WorkEvent::WorkStarted { goal, limits, .. } => format!(
            "WorkStarted (goal: {goal}; max turns {}, max executions {})",
            limits.max_turns, limits.max_executions
        ),
        WorkEvent::Capability(e) => format!("{e:?}"),
        WorkEvent::DecisionStarted { turn, .. } => format!("DecisionStarted (turn {})", turn + 1),
        WorkEvent::LocalDecision { decision, .. } => format!("LocalDecision: {decision}"),
        WorkEvent::ModelEscalation {
            reason, context, ..
        } => format!(
            "ModelEscalation: {reason} (context {} bytes, {} observations, {} decisions)",
            context.bytes, context.observations, context.decisions
        ),
        WorkEvent::DecisionMade { decision, .. } => format!("DecisionMade: {decision}"),
        WorkEvent::CapabilityRequested { capability, .. } => {
            format!("CapabilityRequested: {capability}")
        }
        WorkEvent::Execution(e) => format!("{e:?}"),
        WorkEvent::ObservationRecorded {
            kind, receipt_id, ..
        } => format!(
            "ObservationRecorded: {} (receipt {})",
            kind.as_str(),
            receipt_id.as_deref().unwrap_or("none")
        ),
        WorkEvent::EvidenceRecorded { capability, .. } => format!("EvidenceRecorded: {capability}"),
        WorkEvent::EvidenceReused {
            capability,
            receipt_id,
            ..
        } => format!(
            "EvidenceReused: {capability} (receipt {})",
            receipt_id.as_deref().unwrap_or("none")
        ),
        WorkEvent::WorkCompleted { .. } => "WorkCompleted".to_string(),
        WorkEvent::WorkEscalated { reason, .. } => format!("WorkEscalated: {reason}"),
        WorkEvent::WorkBlocked { reason, .. } => format!("WorkBlocked: {reason}"),
        WorkEvent::WorkLimitReached { limit, .. } => format!("WorkLimitReached: {}", limit.name()),
        WorkEvent::WorkFailed { reason, .. } => format!("WorkFailed: {reason}"),
    }
}

fn print_report(title: &str, report: &WorkReport, model_calls: usize, execute_calls: usize) {
    println!("{title}\n");
    println!("Trajectory:");
    for (i, event) in report.events.iter().enumerate() {
        println!("  {:>2}. {}", i + 1, describe(event));
    }
    let s = &report.summary;
    println!("\nSummary:");
    println!("  turns: {}", s.turns);
    println!(
        "  executions: {} (executor calls observed: {execute_calls})",
        s.executions
    );
    println!("  observations: {}", s.observations);
    println!("  evidence_hits: {}", s.evidence_hits);
    println!("  local_decisions: {}", s.local_decisions);
    println!(
        "  model_escalations: {} (model calls observed: {model_calls})",
        s.model_escalations
    );
    println!("  context_bytes: {}", s.context_bytes);
    println!(
        "  tokens (provider-reported): prompt {}, completion {}",
        s.prompt_tokens, s.completion_tokens
    );
    println!("  elapsed_time: {:?}", s.elapsed);
    println!("  terminal_state: {}", s.terminal_state.name());
    match &report.outcome {
        WorkOutcome::Completed { summary } => println!("  outcome: {summary}"),
        WorkOutcome::Escalated { reason }
        | WorkOutcome::Blocked { reason }
        | WorkOutcome::Failed { reason } => println!("  outcome: {reason}"),
        WorkOutcome::LimitReached { limit } => {
            println!("  outcome: {} limit reached", limit.name())
        }
    }
}

struct Scenario {
    title: &'static str,
    goal: &'static str,
    status: ExecutionStatus,
    limits: WorkLimits,
    policy: Box<dyn LocalWorkPolicy>,
    model_decisions: Vec<WorkDecision>,
    /// Local reasoning verdict for unknown evidence.
    reasoner_continues: bool,
    expect: fn(&WorkReport) -> bool,
}

fn scenario(name: &str) -> Option<Scenario> {
    let goal = "Perform the self test and determine whether it succeeded";
    let limits = WorkLimits {
        max_turns: 6,
        max_executions: 3,
    };
    Some(match name {
        "completion" => Scenario {
            title: "Bounded autonomous work: completion",
            goal,
            status: ExecutionStatus::Success,
            limits,
            policy: Box::new(ReactToObservation),
            model_decisions: vec![],
            reasoner_continues: true,
            expect: |r| {
                matches!(r.outcome, WorkOutcome::Completed { .. })
                    && r.summary.executions == 1
                    && r.summary.turns == 2
            },
        },
        "failure" => Scenario {
            title: "Bounded autonomous work: the first execution fails",
            goal,
            status: ExecutionStatus::Failure,
            limits,
            policy: Box::new(ReactToObservation),
            model_decisions: vec![],
            reasoner_continues: true,
            expect: |r| {
                matches!(r.outcome, WorkOutcome::Blocked { .. })
                    && r.observations[0].kind == ObservationKind::ExecutionFailed
            },
        },
        "limit" => Scenario {
            title: "Bounded autonomous work: a workload that never stops asking",
            goal,
            status: ExecutionStatus::Success,
            limits: WorkLimits {
                max_turns: 10,
                max_executions: 3,
            },
            policy: Box::new(Insatiable),
            model_decisions: vec![],
            reasoner_continues: true,
            expect: |r| {
                r.outcome
                    == WorkOutcome::LimitReached {
                        limit: LimitKind::Executions,
                    }
                    && r.summary.executions == 3
            },
        },
        "turn-limit" => Scenario {
            title: "Bounded autonomous work: asking for the same thing forever, answered by evidence",
            goal,
            status: ExecutionStatus::Success,
            limits: WorkLimits {
                max_turns: 5,
                max_executions: 3,
            },
            policy: Box::new(Repeating),
            model_decisions: vec![],
            reasoner_continues: true,
            expect: |r| {
                r.outcome
                    == WorkOutcome::LimitReached {
                        limit: LimitKind::Turns,
                    }
                    && r.summary.turns == 5
                    && r.summary.executions == 1
                    && r.summary.evidence_hits == 4
            },
        },
        "evidence" => Scenario {
            title: "Bounded autonomous work: valid evidence prevents a second execution",
            goal,
            status: ExecutionStatus::Success,
            limits,
            policy: Box::new(ScriptedPolicy::new(vec![
                Some(WorkDecision::RequestCapability(request("first"))),
                Some(WorkDecision::RequestCapability(request("second"))),
                Some(WorkDecision::Complete {
                    summary: "the evidence answered the second request".into(),
                }),
            ])),
            model_decisions: vec![],
            reasoner_continues: true,
            expect: |r| {
                matches!(r.outcome, WorkOutcome::Completed { .. })
                    && r.summary.executions == 1
                    && r.summary.evidence_hits == 1
                    && r.summary.model_escalations == 0
            },
        },
        "escalation" => Scenario {
            title: "Bounded autonomous work: local reasoning cannot decide, so the model is asked once",
            goal,
            status: ExecutionStatus::Success,
            limits,
            policy: Box::new(ScriptedPolicy::new(vec![
                None,
                Some(WorkDecision::Complete {
                    summary: "completed after the model's request ran".into(),
                }),
            ])),
            model_decisions: vec![WorkDecision::RequestCapability(request("asked-by-model"))],
            reasoner_continues: true,
            expect: |r| {
                matches!(r.outcome, WorkOutcome::Completed { .. })
                    && r.summary.model_escalations == 1
                    && r.summary.executions == 1
            },
        },
        _ => return None,
    })
}

pub const SCENARIOS: [&str; 6] = [
    "completion",
    "failure",
    "limit",
    "turn-limit",
    "evidence",
    "escalation",
];

/// Returns the process exit code.
pub async fn test_work(args: &[String]) -> i32 {
    let name = args.first().map(String::as_str).unwrap_or("completion");
    let Some(s) = scenario(name) else {
        eprintln!(
            "unknown scenario {name:?}; choose one of: {}",
            SCENARIOS.join(", ")
        );
        return 2;
    };
    let model_calls = Arc::new(AtomicUsize::new(0));
    let execute_calls = Arc::new(AtomicUsize::new(0));
    let reasoner = TestLocalReasoner::default().on(
        EvidenceState::Unknown,
        if s.reasoner_continues {
            LocalReasoningResult::Continue {
                rationale: "the demo continues".into(),
            }
        } else {
            LocalReasoningResult::Escalate {
                reason: "the demo escalates".into(),
            }
        },
    );
    let agent = Agent::new(Arc::new(StandInModel(model_calls.clone())))
        .with_capabilities(Arc::new(SelfTestCapability))
        .with_executor(Arc::new(CountingExecutor(
            Arc::new(Fixed(s.status)),
            execute_calls.clone(),
        )))
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(reasoner));
    let spec = WorkSpec::new(WorkId::new(format!("demo-{name}")), WorkGoal::new(s.goal))
        .with_limits(s.limits);
    let report = agent
        .run_work(
            &spec,
            s.policy.as_ref(),
            &ModelDecisions(s.model_decisions, AtomicUsize::new(0)),
        )
        .await;
    print_report(
        s.title,
        &report,
        model_calls.load(Ordering::SeqCst),
        execute_calls.load(Ordering::SeqCst),
    );
    let bounded = report.summary.turns <= s.limits.max_turns
        && report.summary.executions <= s.limits.max_executions;
    if bounded && (s.expect)(&report) {
        println!(
            "\nExpected terminal state reached: {}",
            report.summary.terminal_state.name()
        );
        0
    } else {
        eprintln!("\nFAILED: the workload did not reach its expected terminal state");
        1
    }
}

/// Real Compute, never a fallback. Exit 3 means skipped.
pub async fn test_real_work() -> i32 {
    let compute = Arc::new(chip_compute::ComputeExecutor::new());
    let model_calls = Arc::new(AtomicUsize::new(0));
    let execute_calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(Arc::new(StandInModel(model_calls.clone())))
        .with_capabilities(compute.clone())
        .with_executor(Arc::new(CountingExecutor(compute, execute_calls.clone())))
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default().on(
            EvidenceState::Unknown,
            LocalReasoningResult::Continue {
                rationale: "the demo continues".into(),
            },
        )));
    if let Ok(found) = agent.discover_capabilities().await.result {
        for capability in found {
            if let CapabilityAvailability::Unavailable(reason)
            | CapabilityAvailability::Misconfigured(reason) = capability.availability
            {
                println!("SKIPPED — Compute unavailable ({reason})");
                return 3;
            }
        }
    }
    let spec = WorkSpec::new(
        WorkId::new("real-work"),
        WorkGoal::new("Run the Compute self test and determine whether it succeeded"),
    )
    .with_limits(WorkLimits {
        max_turns: 6,
        max_executions: 2,
    });
    let report = agent
        .run_work(
            &spec,
            &ReactToObservation,
            &ModelDecisions(vec![], AtomicUsize::new(0)),
        )
        .await;
    print_report(
        "Bounded autonomous work (real Compute)",
        &report,
        model_calls.load(Ordering::SeqCst),
        execute_calls.load(Ordering::SeqCst),
    );
    let receipt = report
        .observations
        .first()
        .and_then(|o| o.receipt_id.clone());
    if let Some(WorkEvent::ObservationRecorded {
        kind: ObservationKind::ExecutionFailed,
        ..
    }) = report
        .events
        .iter()
        .find(|e| matches!(e, WorkEvent::ObservationRecorded { .. }))
    {
        eprintln!("\nerror: the real execution failed");
        return 1;
    }
    if report.summary.terminal_state == TerminalState::Completed
        && report.summary.executions == 1
        && receipt.is_some()
    {
        println!(
            "\nExpected terminal state reached: completed (receipt {})",
            receipt.unwrap()
        );
        0
    } else {
        eprintln!(
            "\nerror: the real workload did not complete: {:?}",
            report.outcome
        );
        1
    }
}
