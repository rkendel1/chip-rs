//! PR26: the bounded autonomous work loop. Every decision, execution and observation below happens
//! inside one `run_work` call; the caller never starts a second turn.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, AgentDecision, Capability, CapabilityAvailability, CapabilityDescriptor,
    CapabilityError, CapabilityEvent, CapabilityId, CapabilityInput, CapabilityProvider,
    CapabilityRequest, DecisionBoundary, DecisionError, DecisionInput, EvidenceLookup,
    EvidenceState, ExecutionError, ExecutionEvent, ExecutionId, ExecutionObserver,
    ExecutionRequest, ExecutionResult, ExecutionStatus, Executor, InputValue, LimitKind,
    LocalReasoningResult, LocalWorkPolicy, NoLocalPolicy, ObservationKind, RespondCompletes,
    ScriptedDecision, ScriptedPolicy, StateToken, TerminalState, TestLocalReasoner, WorkDecision,
    WorkDecisionBoundary, WorkEvent, WorkGoal, WorkId, WorkLimits, WorkOutcome, WorkReport,
    WorkSpec, WorkView,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

const SECRET: &str = "sk-live-secret-do-not-leak";

/// A deterministic stand-in for FX. Counts calls, keeps what it was sent, and can fail.
struct Fx {
    calls: AtomicUsize,
    seen: Mutex<Vec<ModelRequest>>,
    fail: bool,
    #[allow(dead_code)]
    api_key: &'static str,
}

impl Fx {
    fn new() -> Arc<Fx> {
        Arc::new(Fx {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            fail: false,
            api_key: SECRET,
        })
    }

    fn failing() -> Arc<Fx> {
        Arc::new(Fx {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            fail: true,
            api_key: SECRET,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ModelProvider for Fx {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(request);
        if self.fail {
            return Err(FxError::Provider("provider unavailable".into()));
        }
        Ok(ModelResponse::new("r", "model reply", Usage::new(7, 3)))
    }
}

/// Executor double with a fixed status and a call counter.
struct Exec {
    calls: AtomicUsize,
    status: ExecutionStatus,
    output: &'static str,
}

impl Exec {
    fn new(status: ExecutionStatus, output: &'static str) -> Arc<Exec> {
        Arc::new(Exec {
            calls: AtomicUsize::new(0),
            status,
            output,
        })
    }

    fn ok() -> Arc<Exec> {
        Self::new(ExecutionStatus::Success, "operation performed")
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(ExecutionResult {
            id: r.id,
            status: self.status,
            output: self.output.to_string(),
            receipt_id: Some(format!("sha256:receipt-{n}")),
        })
    }
}

struct Caps;

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut op = CapabilityDescriptor::new(
            CapabilityId::new("op.perform")?,
            "Perform",
            "Performs the operation",
        );
        op.inputs.push(CapabilityInput {
            name: "n".into(),
            description: "variant".into(),
            required: false,
        });
        Ok(vec![
            CapabilityDescriptor::new(
                CapabilityId::new("compute.selftest")?,
                "Self test",
                "Deterministic",
            ),
            op,
        ])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

fn request(capability: &str, execution: &str) -> CapabilityRequest {
    CapabilityRequest::new(
        ExecutionId::new(execution),
        CapabilityId::new(capability).unwrap(),
    )
}

fn run(capability: &str, execution: &str) -> WorkDecision {
    WorkDecision::RequestCapability(request(capability, execution))
}

fn complete() -> WorkDecision {
    WorkDecision::Complete {
        summary: "done".into(),
    }
}

/// Model-side boundary that plays back a fixed list of decisions, one per escalation.
struct ModelScript {
    decisions: Vec<WorkDecision>,
    next: AtomicUsize,
}

impl ModelScript {
    fn new(decisions: Vec<WorkDecision>) -> ModelScript {
        ModelScript {
            decisions,
            next: AtomicUsize::new(0),
        }
    }
}

impl WorkDecisionBoundary for ModelScript {
    fn interpret(
        &self,
        _r: &ModelResponse,
        _c: &[Capability],
    ) -> Result<WorkDecision, DecisionError> {
        let i = self.next.fetch_add(1, Ordering::SeqCst);
        self.decisions
            .get(i)
            .cloned()
            .ok_or_else(|| DecisionError::InvalidDecision("the script ran out".into()))
    }
}

/// The reasoner local policy: continue whatever the evidence state.
fn permissive() -> Arc<TestLocalReasoner> {
    Arc::new(
        TestLocalReasoner::default()
            .on(
                EvidenceState::Unknown,
                LocalReasoningResult::Continue {
                    rationale: "ok".into(),
                },
            )
            .on(
                EvidenceState::KnownStale,
                LocalReasoningResult::Continue {
                    rationale: "ok".into(),
                },
            ),
    )
}

fn agent(fx: &Arc<Fx>, exec: &Arc<Exec>, reasoner: Arc<TestLocalReasoner>) -> Agent {
    Agent::new(fx.clone())
        .with_capabilities(Arc::new(Caps))
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(reasoner)
}

fn spec(goal: &str) -> WorkSpec {
    WorkSpec::new(WorkId::new("work-1"), WorkGoal::new(goal))
}

fn names(report: &WorkReport) -> Vec<String> {
    report
        .events
        .iter()
        .map(|e| match e {
            WorkEvent::GoalEvaluated { satisfied, .. } => format!("GoalEvaluated {satisfied}"),
            WorkEvent::WorkStarted { .. } => "WorkStarted".to_string(),
            WorkEvent::Capability(CapabilityEvent::CapabilitiesRequested) => {
                "CapabilitiesRequested".into()
            }
            WorkEvent::Capability(CapabilityEvent::CapabilitiesAvailable { .. }) => {
                "CapabilitiesAvailable".into()
            }
            WorkEvent::Capability(_) => "CapabilitiesUnavailable".into(),
            WorkEvent::DecisionStarted { turn, .. } => format!("DecisionStarted {turn}"),
            WorkEvent::LocalDecision { decision, .. } => format!("LocalDecision {decision}"),
            WorkEvent::ModelEscalation { .. } => "ModelEscalation".into(),
            WorkEvent::ModelCalled { .. } => "ModelCalled".into(),
            WorkEvent::ContextLimit { .. } => "ContextLimit".into(),
            WorkEvent::DecisionMade { decision, .. } => format!("DecisionMade {decision}"),
            WorkEvent::CapabilityRequested { capability, .. } => {
                format!("CapabilityRequested {capability}")
            }
            WorkEvent::Execution(ExecutionEvent::ExecutionRequested { .. }) => {
                "ExecutionRequested".into()
            }
            WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. }) => {
                "ExecutionStarted".into()
            }
            WorkEvent::Execution(ExecutionEvent::ExecutionCompleted { .. }) => {
                "ExecutionCompleted".into()
            }
            WorkEvent::Execution(ExecutionEvent::ExecutionFailed { .. }) => {
                "ExecutionFailed".into()
            }
            WorkEvent::ObservationRecorded { .. } => "ObservationRecorded".into(),
            WorkEvent::EvidenceRecorded { .. } => "EvidenceRecorded".into(),
            WorkEvent::EvidenceReused { .. } => "EvidenceReused".into(),
            WorkEvent::WorkCompleted { .. } => "WorkCompleted".into(),
            WorkEvent::WorkEscalated { .. } => "WorkEscalated".into(),
            WorkEvent::WorkBlocked { .. } => "WorkBlocked".into(),
            WorkEvent::WorkLimitReached { .. } => "WorkLimitReached".into(),
            WorkEvent::WorkFailed { .. } => "WorkFailed".into(),
        })
        .collect()
}

// ------------------------------------------------------------------------------------------
// Core behavior
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn one_autonomous_workload_decides_executes_observes_and_decides_again() {
    let (fx, exec) = (Fx::new(), Exec::ok());
    let agent = agent(&fx, &exec, permissive());
    let policy = ScriptedPolicy::new(vec![Some(run("compute.selftest", "e1")), Some(complete())]);

    // One call. The second decision is made inside it.
    let report = agent
        .run_work(
            &spec("perform the operation and report whether it succeeded"),
            &policy,
            &ModelScript::new(vec![]),
        )
        .await;

    assert_eq!(
        report.outcome,
        WorkOutcome::Completed {
            summary: "done".into()
        }
    );
    assert_eq!(
        names(&report),
        [
            "WorkStarted",
            "CapabilitiesRequested",
            "CapabilitiesAvailable",
            "DecisionStarted 0",
            "LocalDecision request compute.selftest",
            "DecisionMade request compute.selftest",
            "CapabilityRequested compute.selftest",
            "ExecutionRequested",
            "ExecutionStarted",
            "ExecutionCompleted",
            "ObservationRecorded",
            "EvidenceRecorded",
            "DecisionStarted 1",
            "LocalDecision complete",
            "DecisionMade complete",
            "WorkCompleted",
        ]
    );
    let s = report.summary;
    assert_eq!(
        (
            s.turns,
            s.executions,
            s.observations,
            s.evidence_hits,
            s.local_decisions,
            s.model_escalations,
            s.context_bytes
        ),
        (2, 1, 1, 0, 2, 0, 0)
    );
    assert_eq!(s.terminal_state, TerminalState::Completed);
    assert_eq!((exec.calls(), fx.calls()), (1, 0), "no model was needed");
    assert_eq!(report.observations.len(), 1);
    assert_eq!(
        report.observations[0].kind,
        ObservationKind::ExecutionCompleted
    );
    assert_eq!(
        report.observations[0].receipt_id.as_deref(),
        Some("sha256:receipt-1")
    );
}

/// Decides from what the loop has observed: the observation is what drives the next decision.
struct Reactive;

impl LocalWorkPolicy for Reactive {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        match view.observations.last() {
            None => Some(run("compute.selftest", "e1")),
            Some(o) if o.kind == ObservationKind::ExecutionCompleted => {
                Some(WorkDecision::Complete {
                    summary: "it succeeded".into(),
                })
            }
            Some(o) if o.kind == ObservationKind::ExecutionFailed => Some(WorkDecision::Block {
                reason: format!("it failed ({})", o.receipt_id.clone().unwrap_or_default()),
            }),
            Some(_) => Some(WorkDecision::Escalate {
                reason: "cancelled".into(),
            }),
        }
    }
}

#[tokio::test]
async fn the_next_decision_is_made_from_the_observation() {
    let (fx, exec) = (Fx::new(), Exec::ok());
    let report = agent(&fx, &exec, permissive())
        .run_work(&spec("g"), &Reactive, &ModelScript::new(vec![]))
        .await;
    assert_eq!(
        report.outcome,
        WorkOutcome::Completed {
            summary: "it succeeded".into()
        }
    );
    assert_eq!(report.decisions.len(), 2);
}

#[tokio::test]
async fn a_failed_execution_is_observed_and_the_next_decision_follows_the_failure() {
    let (fx, exec) = (
        Fx::new(),
        Exec::new(ExecutionStatus::Failure, "the operation failed"),
    );
    let report = agent(&fx, &exec, permissive())
        .run_work(&spec("g"), &Reactive, &ModelScript::new(vec![]))
        .await;

    // No fabricated recovery: the failure is observed, and the decision is the policy's reaction.
    assert_eq!(
        report.outcome,
        WorkOutcome::Blocked {
            reason: "it failed (sha256:receipt-1)".into()
        }
    );
    assert_eq!(
        report.observations[0].kind,
        ObservationKind::ExecutionFailed
    );
    assert_eq!(report.observations[0].status, ExecutionStatus::Failure);
    assert!(names(&report).contains(&"ExecutionFailed".to_string()));
    assert!(!names(&report).contains(&"ExecutionCompleted".to_string()));
    assert_eq!(report.summary.terminal_state, TerminalState::Blocked);
    assert_eq!(exec.calls(), 1, "no retry");
}

#[tokio::test]
async fn each_terminal_state_is_reachable_and_explicit() {
    let (fx, exec) = (Fx::new(), Exec::ok());
    let a = agent(&fx, &exec, permissive());
    let no_model = ModelScript::new(vec![]);
    let escalated = a
        .run_work(
            &spec("g"),
            &ScriptedPolicy::new(vec![Some(WorkDecision::Escalate {
                reason: "needs a person".into(),
            })]),
            &no_model,
        )
        .await;
    assert_eq!(
        escalated.outcome,
        WorkOutcome::Escalated {
            reason: "needs a person".into()
        }
    );
    assert!(names(&escalated).contains(&"WorkEscalated".to_string()));

    let blocked = a
        .run_work(
            &spec("g"),
            &ScriptedPolicy::new(vec![Some(WorkDecision::Block {
                reason: "no approval".into(),
            })]),
            &no_model,
        )
        .await;
    assert_eq!(
        blocked.outcome,
        WorkOutcome::Blocked {
            reason: "no approval".into()
        }
    );

    // An undeclared capability blocks; it never reaches the executor.
    let unknown = a
        .run_work(
            &spec("g"),
            &ScriptedPolicy::new(vec![Some(run("not.declared", "x"))]),
            &no_model,
        )
        .await;
    assert!(
        matches!(unknown.outcome, WorkOutcome::Blocked { .. }),
        "{:?}",
        unknown.outcome
    );
    assert_eq!(exec.calls(), 0);

    // No executor at all: blocked, not failed-as-success.
    let bare = Agent::new(fx.clone())
        .with_capabilities(Arc::new(Caps))
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(permissive());
    let no_executor = bare
        .run_work(
            &spec("g"),
            &ScriptedPolicy::new(vec![Some(run("compute.selftest", "x"))]),
            &no_model,
        )
        .await;
    assert!(
        matches!(no_executor.outcome, WorkOutcome::Blocked { .. }),
        "{:?}",
        no_executor.outcome
    );

    // An empty goal is a failure, not a success.
    let empty = a
        .run_work(
            &spec("   "),
            &ScriptedPolicy::new(vec![Some(complete())]),
            &no_model,
        )
        .await;
    assert!(matches!(empty.outcome, WorkOutcome::Failed { .. }));
    assert_eq!(empty.summary.terminal_state, TerminalState::Failed);
}

// ------------------------------------------------------------------------------------------
// Bounds
// ------------------------------------------------------------------------------------------

/// Always asks for another capability, with different inputs each time so evidence never answers.
struct Insatiable;

impl LocalWorkPolicy for Insatiable {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        Some(WorkDecision::RequestCapability(
            request("op.perform", &format!("e{}", view.turn))
                .with_input("n", InputValue::Integer(view.turn as i64)),
        ))
    }
}

/// Always asks for the very same thing, so evidence answers every time after the first.
struct Repetitive;

impl LocalWorkPolicy for Repetitive {
    fn propose(&self, _view: &WorkView<'_>) -> Option<WorkDecision> {
        Some(run("compute.selftest", "same"))
    }
}

#[tokio::test]
async fn the_execution_limit_stops_the_loop_and_is_never_exceeded() {
    let (fx, exec) = (Fx::new(), Exec::ok());
    let limits = WorkLimits {
        max_turns: 10,
        max_executions: 3,
    };
    let report = agent(&fx, &exec, permissive())
        .run_work(
            &spec("g").with_limits(limits),
            &Insatiable,
            &ModelScript::new(vec![]),
        )
        .await;
    assert_eq!(
        report.outcome,
        WorkOutcome::LimitReached {
            limit: LimitKind::Executions
        }
    );
    assert_eq!(exec.calls(), 3);
    assert_eq!(report.summary.executions, 3);
    assert!(report.summary.turns <= 10);
    assert!(names(&report).contains(&"WorkLimitReached".to_string()));
    assert_ne!(report.summary.terminal_state, TerminalState::Completed);
}

#[tokio::test]
async fn the_turn_limit_stops_a_loop_even_when_evidence_means_nothing_executes() {
    let (fx, exec) = (Fx::new(), Exec::ok());
    let limits = WorkLimits {
        max_turns: 5,
        max_executions: 100,
    };
    let report = agent(&fx, &exec, permissive())
        .run_work(
            &spec("g").with_limits(limits),
            &Repetitive,
            &ModelScript::new(vec![]),
        )
        .await;
    assert_eq!(
        report.outcome,
        WorkOutcome::LimitReached {
            limit: LimitKind::Turns
        }
    );
    assert_eq!(report.summary.turns, 5);
    assert_eq!(
        exec.calls(),
        1,
        "the same request is answered by evidence after the first"
    );
    assert_eq!(report.summary.evidence_hits, 4);
}

#[tokio::test]
async fn limits_hold_for_every_combination_and_every_run_ends_terminal() {
    for max_turns in 0..6 {
        for max_executions in 0..5 {
            let (fx, exec) = (Fx::new(), Exec::ok());
            let limits = WorkLimits {
                max_turns,
                max_executions,
            };
            let report = agent(&fx, &exec, permissive())
                .run_work(
                    &spec("g").with_limits(limits),
                    &Insatiable,
                    &ModelScript::new(vec![]),
                )
                .await;
            assert!(
                report.summary.turns <= max_turns,
                "{max_turns}/{max_executions}: {} turns",
                report.summary.turns
            );
            assert!(
                exec.calls() <= max_executions,
                "{max_turns}/{max_executions}: {} executions",
                exec.calls()
            );
            assert!(matches!(report.outcome, WorkOutcome::LimitReached { .. }));
            assert_eq!(report.summary.executions, exec.calls());
            assert_eq!(fx.calls(), 0);
        }
    }
    // Zero turns means no decision at all.
    let (fx, exec) = (Fx::new(), Exec::ok());
    let report = agent(&fx, &exec, permissive())
        .run_work(
            &spec("g").with_limits(WorkLimits {
                max_turns: 0,
                max_executions: 5,
            }),
            &Insatiable,
            &ModelScript::new(vec![]),
        )
        .await;
    assert_eq!(
        (report.summary.turns, report.outcome),
        (
            0,
            WorkOutcome::LimitReached {
                limit: LimitKind::Turns
            }
        )
    );
    // The defaults are finite.
    let d = WorkLimits::default();
    assert!(d.max_turns > 0 && d.max_executions > 0 && d.max_turns < 1000);
}

// ------------------------------------------------------------------------------------------
// Evidence
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn valid_evidence_is_reused_inside_the_loop_without_executing_or_calling_the_model() {
    let (fx, exec) = (Fx::new(), Exec::ok());
    let policy = ScriptedPolicy::new(vec![
        Some(run("compute.selftest", "first")),
        Some(run("compute.selftest", "second")),
        Some(complete()),
    ]);
    let report = agent(&fx, &exec, permissive())
        .run_work(&spec("g"), &policy, &ModelScript::new(vec![]))
        .await;

    assert_eq!(
        report.outcome,
        WorkOutcome::Completed {
            summary: "done".into()
        }
    );
    assert_eq!(
        exec.calls(),
        1,
        "the second identical request did not execute"
    );
    assert_eq!(fx.calls(), 0, "no model call");
    assert_eq!(report.summary.evidence_hits, 1);
    assert_eq!(report.summary.executions, 1);
    assert_eq!(report.summary.turns, 3, "the loop continued");

    // The receipt of the original execution stays attached to the reused evidence.
    let reused = report.events.iter().find_map(|e| match e {
        WorkEvent::EvidenceReused { receipt_id, .. } => Some(receipt_id.clone()),
        _ => None,
    });
    assert_eq!(reused, Some(Some("sha256:receipt-1".to_string())));
    assert_eq!(report.observations.len(), 2);
    assert_eq!(
        report.observations[1].receipt_id.as_deref(),
        Some("sha256:receipt-1")
    );
    let n = names(&report);
    let first_reuse = n.iter().position(|x| x == "EvidenceReused").unwrap();
    assert_eq!(
        &n[first_reuse - 2..=first_reuse],
        [
            "DecisionMade request compute.selftest",
            "CapabilityRequested compute.selftest",
            "EvidenceReused"
        ]
    );
}

#[tokio::test]
async fn evidence_for_a_model_proposed_request_is_reused_too() {
    let (fx, exec) = (Fx::new(), Exec::ok());
    // Turn 1 local, turn 2 forced to the model, which asks for the same thing again.
    let policy = ScriptedPolicy::new(vec![
        Some(run("compute.selftest", "first")),
        None,
        Some(complete()),
    ]);
    let model = ModelScript::new(vec![run("compute.selftest", "again")]);
    let report = agent(&fx, &exec, permissive())
        .run_work(&spec("g"), &policy, &model)
        .await;
    assert_eq!(
        (exec.calls(), fx.calls(), report.summary.evidence_hits),
        (1, 1, 1)
    );
    assert!(matches!(report.outcome, WorkOutcome::Completed { .. }));
}

#[tokio::test]
async fn stale_evidence_is_not_reused_it_goes_to_local_reasoning() {
    let (fx, exec) = (Fx::new(), Exec::ok());
    let state_a = StateToken::new("state-a");
    let state_b = StateToken::new("state-b");
    let first = ScriptedPolicy::new(vec![Some(run("compute.selftest", "a")), Some(complete())]);

    // Same agent, so the evidence store persists: evidence established under state A.
    let a = agent(&fx, &exec, permissive());
    a.run_work(
        &spec("g").with_state(state_a.clone()),
        &first,
        &ModelScript::new(vec![]),
    )
    .await;
    assert_eq!(exec.calls(), 1);

    // Under state B it is stale: not reused. The permissive reasoner lets it run again.
    let second = ScriptedPolicy::new(vec![Some(run("compute.selftest", "b")), Some(complete())]);
    let report = a
        .run_work(
            &spec("g").with_state(state_b.clone()),
            &second,
            &ModelScript::new(vec![]),
        )
        .await;
    assert_eq!((exec.calls(), report.summary.evidence_hits), (2, 0));

    // With a reasoner that escalates stale evidence, it goes to the model instead.
    let (fx2, exec2) = (Fx::new(), Exec::ok());
    let strict = agent(
        &fx2,
        &exec2,
        Arc::new(TestLocalReasoner::default().on(
            EvidenceState::Unknown,
            LocalReasoningResult::Continue {
                rationale: "ok".into(),
            },
        )),
    );
    strict
        .run_work(
            &spec("g").with_state(state_a),
            &first,
            &ModelScript::new(vec![]),
        )
        .await;
    let policy = ScriptedPolicy::new(vec![Some(run("compute.selftest", "b"))]);
    let model = ModelScript::new(vec![complete()]);
    let escalated = strict
        .run_work(&spec("g").with_state(state_b), &policy, &model)
        .await;
    assert_eq!(
        (exec2.calls(), fx2.calls(), escalated.summary.evidence_hits),
        (1, 1, 0)
    );
    assert!(matches!(escalated.outcome, WorkOutcome::Completed { .. }));
}

#[tokio::test]
async fn unknown_evidence_follows_local_reasoning() {
    // Continue: the request proceeds locally. Escalate: the model is asked. Neither reuses anything.
    let (fx, exec) = (Fx::new(), Exec::ok());
    let go = agent(&fx, &exec, permissive())
        .run_work(
            &spec("g"),
            &ScriptedPolicy::new(vec![Some(run("compute.selftest", "u")), Some(complete())]),
            &ModelScript::new(vec![]),
        )
        .await;
    assert_eq!(
        (
            go.summary.local_decisions,
            go.summary.model_escalations,
            exec.calls(),
            fx.calls()
        ),
        (2, 0, 1, 0)
    );

    let (fx, exec) = (Fx::new(), Exec::ok());
    let default = Arc::new(TestLocalReasoner::default());
    let asked = agent(&fx, &exec, default)
        .run_work(
            &spec("g"),
            &ScriptedPolicy::new(vec![Some(run("compute.selftest", "u"))]),
            &ModelScript::new(vec![complete()]),
        )
        .await;
    assert_eq!(
        (asked.summary.model_escalations, fx.calls(), exec.calls()),
        (1, 1, 0)
    );
    let reason = asked.events.iter().find_map(|e| match e {
        WorkEvent::ModelEscalation { reason, .. } => Some(reason.clone()),
        _ => None,
    });
    assert!(reason.unwrap().contains("no evidence"));
}

#[tokio::test]
async fn a_cancelled_execution_is_observed_but_never_becomes_evidence() {
    let (fx, exec) = (Fx::new(), Exec::new(ExecutionStatus::Cancelled, ""));
    let a = agent(&fx, &exec, permissive());
    let policy = ScriptedPolicy::new(vec![
        Some(run("compute.selftest", "c1")),
        Some(run("compute.selftest", "c2")),
        Some(complete()),
    ]);
    let report = a
        .run_work(&spec("g"), &policy, &ModelScript::new(vec![]))
        .await;

    assert_eq!(
        report.observations[0].kind,
        ObservationKind::ExecutionCancelled
    );
    assert!(!names(&report).contains(&"EvidenceRecorded".to_string()));
    assert_eq!(
        a.lookup_evidence(&request("compute.selftest", "x")),
        EvidenceLookup::NotFound
    );
    assert_eq!(exec.calls(), 2, "nothing was reusable, so it ran again");
    assert_eq!(report.summary.evidence_hits, 0);
}

#[tokio::test]
async fn a_failure_is_evidence_and_is_reused_like_any_established_outcome() {
    let (fx, exec) = (Fx::new(), Exec::new(ExecutionStatus::Failure, "boom"));
    let policy = ScriptedPolicy::new(vec![
        Some(run("compute.selftest", "f1")),
        Some(run("compute.selftest", "f2")),
        Some(complete()),
    ]);
    let report = agent(&fx, &exec, permissive())
        .run_work(&spec("g"), &policy, &ModelScript::new(vec![]))
        .await;
    assert_eq!(
        exec.calls(),
        1,
        "the established failure was not rediscovered"
    );
    assert_eq!(
        report.observations[1].kind,
        ObservationKind::ExecutionFailed
    );
    assert_eq!(report.summary.evidence_hits, 1);
}

// ------------------------------------------------------------------------------------------
// Reality
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn model_output_cannot_manufacture_execution_or_evidence() {
    struct Boasts;
    #[async_trait::async_trait]
    impl ModelProvider for Boasts {
        async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
            Ok(ModelResponse::new(
                "r",
                "The deployment succeeded.",
                Usage::new(1, 1),
            ))
        }
    }
    let exec = Exec::ok();
    let agent = Agent::new(Arc::new(Boasts))
        .with_capabilities(Arc::new(Caps))
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()));
    // The boundary reads the model's text as a plain response: "complete".
    let boundary = RespondCompletes(ScriptedDecision::new(DecisionInput::Respond));
    let report = agent
        .run_work(&spec("deploy and confirm"), &NoLocalPolicy, &boundary)
        .await;

    assert!(
        matches!(&report.outcome, WorkOutcome::Completed { summary } if summary == "The deployment succeeded.")
    );
    assert_eq!(exec.calls(), 0, "nothing was executed");
    assert!(report.observations.is_empty());
    assert_eq!(report.summary.observations, 0);
    assert!(!names(&report).iter().any(|n| n.starts_with("Execution")
        || n == "ObservationRecorded"
        || n == "EvidenceRecorded"));
    assert_eq!(agent.evidence_stats().hits, 0);
    assert_eq!(
        agent.lookup_evidence(&request("compute.selftest", "x")),
        EvidenceLookup::NotFound
    );
}

#[tokio::test]
async fn a_capability_request_is_not_an_execution() {
    // Requested, but the execution budget is zero: it is asked for and never performed.
    let (fx, exec) = (Fx::new(), Exec::ok());
    let limits = WorkLimits {
        max_turns: 3,
        max_executions: 0,
    };
    let report = agent(&fx, &exec, permissive())
        .run_work(
            &spec("g").with_limits(limits),
            &ScriptedPolicy::new(vec![Some(run("compute.selftest", "r"))]),
            &ModelScript::new(vec![]),
        )
        .await;
    assert!(names(&report).contains(&"CapabilityRequested compute.selftest".to_string()));
    assert_eq!(exec.calls(), 0);
    assert!(report.observations.is_empty());
    assert_eq!(
        report.outcome,
        WorkOutcome::LimitReached {
            limit: LimitKind::Executions
        }
    );
}

#[tokio::test]
async fn the_execution_result_is_authoritative_whatever_the_policy_or_model_expected() {
    // The model says "success" in words; the executor reports failure. The observation says failure.
    let (fx, exec) = (
        Fx::new(),
        Exec::new(ExecutionStatus::Failure, "it did not work"),
    );
    let policy = ScriptedPolicy::new(vec![None, Some(complete())]);
    let model = ModelScript::new(vec![run("compute.selftest", "m1")]);
    let a = agent(&fx, &exec, Arc::new(TestLocalReasoner::default()));
    let report = a.run_work(&spec("g"), &policy, &model).await;
    assert_eq!(
        report.observations[0].kind,
        ObservationKind::ExecutionFailed
    );
    assert_eq!(
        report.observations[0].output.as_deref(),
        Some("it did not work")
    );
    assert_eq!(
        report.observations[0].receipt_id.as_deref(),
        Some("sha256:receipt-1")
    );
    // The evidence store holds the observation of the execution, not the model's wording.
    match a.lookup_evidence(&request("compute.selftest", "z")) {
        EvidenceLookup::Found(o) => assert_eq!(o.status, ExecutionStatus::Failure),
        other => panic!("{other:?}"),
    }
}

// ------------------------------------------------------------------------------------------
// Escalation
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_escalation_makes_exactly_one_model_call_and_measures_what_it_sent() {
    let (fx, exec) = (Fx::new(), Exec::ok());
    let policy = ScriptedPolicy::new(vec![None, Some(complete())]);
    let model = ModelScript::new(vec![run("compute.selftest", "m1")]);
    let report = agent(&fx, &exec, permissive())
        .run_work(&spec("check the thing"), &policy, &model)
        .await;

    assert_eq!(fx.calls(), 1, "one call, no retry");
    assert_eq!(report.summary.model_escalations, 1);
    assert_eq!(
        exec.calls(),
        1,
        "the model's decision was incorporated: its request ran"
    );
    assert!(
        matches!(report.outcome, WorkOutcome::Completed { .. }),
        "and the loop continued from it"
    );
    let n = names(&report);
    let i = n.iter().position(|x| x == "ModelEscalation").unwrap();
    assert_eq!(n[i - 1], "DecisionStarted 0");
    assert_eq!(n[i + 1], "ModelCalled", "the one model call");
    assert_eq!(n[i + 2], "DecisionMade request compute.selftest");

    // The recorded measurement matches the bytes the provider actually received.
    let sent = fx.seen.lock().unwrap()[0].clone();
    let actual: usize = sent.messages.iter().map(|m| m.content.len()).sum();
    let metrics = report.escalations[0];
    assert_eq!(metrics.bytes, actual);
    assert_eq!(
        metrics.chars,
        sent.messages
            .iter()
            .map(|m| m.content.chars().count())
            .sum::<usize>()
    );
    assert_eq!(
        (
            metrics.observations,
            metrics.decisions,
            metrics.evidence_items,
            metrics.ruled_out
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(report.summary.context_bytes, actual);
    assert_eq!(
        (
            report.summary.prompt_tokens,
            report.summary.completion_tokens
        ),
        (7, 3),
        "the provider's own token counts"
    );
    assert!(
        sent.messages
            .last()
            .unwrap()
            .content
            .contains("Goal: check the thing")
    );
}

#[tokio::test]
async fn a_later_escalation_carries_only_the_relevant_history() {
    let (fx, exec) = (Fx::new(), Exec::new(ExecutionStatus::Failure, "no luck"));
    let policy = ScriptedPolicy::new(vec![Some(run("compute.selftest", "e1")), None]);
    let model = ModelScript::new(vec![WorkDecision::Block {
        reason: "cannot proceed".into(),
    }]);
    let report = agent(&fx, &exec, permissive())
        .run_work(&spec("g"), &policy, &model)
        .await;
    assert_eq!(
        report.outcome,
        WorkOutcome::Blocked {
            reason: "cannot proceed".into()
        }
    );
    let m = report.escalations[0];
    assert_eq!((m.observations, m.decisions, m.ruled_out), (1, 1, 1));
    let sent = fx.seen.lock().unwrap()[0].clone();
    let text: Vec<&str> = sent.messages.iter().map(|m| m.content.as_str()).collect();
    assert!(
        text.iter().any(|t| t.contains("kind: execution.failed")),
        "the real observation is handed over"
    );
    assert!(
        text.last()
            .unwrap()
            .contains("capability compute.selftest: its execution failed")
    );
    assert!(
        text.last()
            .unwrap()
            .contains("turn 1 (local): request compute.selftest")
    );
}

#[tokio::test]
async fn the_event_stream_carries_no_secrets_and_no_prompt() {
    let (fx, exec) = (Fx::new(), Exec::ok());
    let policy = ScriptedPolicy::new(vec![None, Some(complete())]);
    let model = ModelScript::new(vec![run("compute.selftest", "m1")]);
    let report = agent(&fx, &exec, permissive())
        .run_work(&spec("sensitive goal text"), &policy, &model)
        .await;
    let events = format!("{:?}", report.events);
    assert!(!events.contains(SECRET));
    let escalation = report
        .events
        .iter()
        .find(|e| matches!(e, WorkEvent::ModelEscalation { .. }))
        .unwrap();
    let text = format!("{escalation:?}");
    assert!(
        !text.contains("sensitive goal text")
            && !text.contains("Question:")
            && !text.contains("Goal:"),
        "{text}"
    );
}

#[tokio::test]
async fn a_provider_failure_is_a_failure_after_exactly_one_call() {
    let (fx, exec) = (Fx::failing(), Exec::ok());
    let report = agent(&fx, &exec, permissive())
        .run_work(
            &spec("g"),
            &NoLocalPolicy,
            &ModelScript::new(vec![complete()]),
        )
        .await;
    assert!(matches!(report.outcome, WorkOutcome::Failed { .. }));
    assert_eq!(fx.calls(), 1, "no retry");
    assert_eq!(report.summary.terminal_state, TerminalState::Failed);
    assert!(!format!("{:?}", report.events).contains(SECRET));
}

#[tokio::test]
async fn a_model_response_that_is_not_a_valid_decision_fails_the_work() {
    let (fx, exec) = (Fx::new(), Exec::ok());
    let report = agent(&fx, &exec, permissive())
        .run_work(&spec("g"), &NoLocalPolicy, &ModelScript::new(vec![]))
        .await;
    assert!(
        matches!(&report.outcome, WorkOutcome::Failed { reason } if reason.contains("not a valid decision"))
    );
    assert_eq!((fx.calls(), exec.calls()), (1, 0));
}

#[tokio::test]
async fn the_boundary_adapter_keeps_requests_as_requests() {
    let (fx, exec) = (Fx::new(), Exec::ok());
    struct Asks;
    impl DecisionBoundary for Asks {
        fn decide(
            &self,
            response: &ModelResponse,
            caps: &[Capability],
        ) -> Result<AgentDecision, DecisionError> {
            ScriptedDecision::new(DecisionInput::RequestCapability {
                execution_id: ExecutionId::new("viaboundary"),
                capability_id: "compute.selftest".into(),
                inputs: Default::default(),
            })
            .decide(response, caps)
        }
    }
    let policy = ScriptedPolicy::new(vec![None, Some(complete())]);
    let report = agent(&fx, &exec, permissive())
        .run_work(&spec("g"), &policy, &RespondCompletes(Asks))
        .await;
    assert_eq!((exec.calls(), fx.calls()), (1, 1));
    assert!(matches!(report.outcome, WorkOutcome::Completed { .. }));
}

// ------------------------------------------------------------------------------------------
// Determinism and discovery
// ------------------------------------------------------------------------------------------

async fn mixed_run() -> WorkReport {
    let (fx, exec) = (Fx::new(), Exec::ok());
    let policy = ScriptedPolicy::new(vec![
        Some(run("compute.selftest", "a")),
        None,
        Some(run("compute.selftest", "b")),
        Some(complete()),
    ]);
    let model = ModelScript::new(vec![run("op.perform", "m")]);
    agent(&fx, &exec, permissive())
        .run_work(&spec("g"), &policy, &model)
        .await
}

#[tokio::test]
async fn identical_workloads_produce_identical_trajectories() {
    let a = mixed_run().await;
    let b = mixed_run().await;
    assert_eq!(a.events, b.events);
    assert_eq!(a.outcome, b.outcome);
    assert_eq!(a.decisions, b.decisions);
    assert_eq!(a.observations, b.observations);
    assert_eq!(a.escalations, b.escalations);
    let strip = |mut r: WorkReport| {
        r.summary.elapsed = Default::default();
        r.summary
    };
    assert_eq!(
        strip(a),
        strip(b),
        "counts agree; only elapsed time may differ"
    );
}

#[tokio::test]
async fn capabilities_are_described_once_not_per_iteration() {
    let report = mixed_run().await;
    let discoveries = names(&report)
        .iter()
        .filter(|n| *n == "CapabilitiesRequested")
        .count();
    assert_eq!(discoveries, 1);
    assert!(report.summary.turns >= 4);
}
