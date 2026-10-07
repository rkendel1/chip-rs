//! PR38: execution success is not goal satisfaction, and Chip recovers from a wrong judgment
//! through authoritative evidence without ever treating the wrong execution as completed work.
//!
//! Every observation here comes from real Compute: the real `ComputeExecutor` runs the real
//! `compute` binary, and receipts are Compute's. Only the *model* is scripted (a fixture that
//! replies with fixed text), which is how a wrong judgment is made deterministic. Nothing here
//! simulates an execution, an observation, evidence or a receipt. If Compute is not installed
//! each test says SKIPPED and does nothing.
//!
//! The opaque assignment is the default: `compute.op_a` computes the SHA-256 digest of the fixed
//! test input (the goal's required output), `compute.op_b` reports the runtime, `compute.op_c`
//! runs the self test. All three execute successfully; only one satisfies the goal.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chip_compute::{ComputeExecutor, HASH_EXPECTED_SHA256, OP_A_INTENT, OP_B_INTENT, OP_C_INTENT};
use chip_core::{
    Agent, CapabilityAvailability, CapabilityId, CapabilityProvider, CapabilityRequest,
    EvidenceLookup, ExecutionEvent, ExecutionId, ExecutionObserver, LimitKind, LocalWorkPolicy,
    ModelDecisionBoundary, ObservationKind, TestLocalReasoner, WorkDecision, WorkEvent, WorkGoal,
    WorkId, WorkLimits, WorkOutcome, WorkReport, WorkSpec, WorkView, verify_trajectory,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

const RIGHT: &str = OP_A_INTENT;
const RUNTIME_INFO: &str = OP_B_INTENT;
const SELF_TEST: &str = OP_C_INTENT;
const GOAL: &str = "Determine the SHA-256 digest of the fixed test input.";

/// Replies with scripted text in order, and keeps everything it was sent. A call beyond the script
/// is an error, so an unexpected extra model call fails the run instead of being papered over.
struct Script {
    replies: Mutex<VecDeque<String>>,
    sent: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ModelProvider for Script {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        let n = {
            let mut sent = self.sent.lock().unwrap();
            sent.push(
                request
                    .messages
                    .iter()
                    .map(|m| m.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            sent.len()
        };
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| FxError::Provider("unscripted model call".into()))?;
        Ok(ModelResponse::new(
            format!("msg{n}"),
            reply,
            Usage::new(5, 5),
        ))
    }
}

/// Ask the model first; afterwards propose to complete with what was observed. The summary is the
/// last observation's output, never anything a model said. The loop, not this policy, decides
/// whether a completion is allowed.
struct ReportObservation;

impl LocalWorkPolicy for ReportObservation {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        if view.turn == 0 {
            return None;
        }
        view.observations
            .last()
            .filter(|o| o.kind == ObservationKind::ExecutionCompleted)
            .map(|o| WorkDecision::Complete {
                summary: format!(
                    "{} (receipt {})",
                    o.output.clone().unwrap_or_default(),
                    o.receipt_id.clone().unwrap_or_default()
                ),
            })
    }
}

fn request(capability: &str) -> String {
    format!(
        r#"{{"schema":"chip.work-decision.v1","decision":"request_capability","capability":"{capability}"}}"#
    )
}

struct Run {
    report: WorkReport,
    script: Arc<Script>,
    agent: Agent,
    limits: WorkLimits,
}

/// `None` (after saying so) when Compute is not available here.
async fn run_with(replies: &[String], limits: WorkLimits, require: bool) -> Option<Run> {
    let compute = ComputeExecutor::new().with_opaque_operations();
    let probe = CapabilityId::new(RIGHT).unwrap();
    if !matches!(
        compute.availability(&probe).await,
        CapabilityAvailability::Available
    ) {
        eprintln!("SKIPPED: Compute is not available");
        return None;
    }
    let script = Arc::new(Script {
        replies: Mutex::new(replies.iter().cloned().collect()),
        sent: Mutex::new(vec![]),
    });
    let agent = Agent::new(script.clone())
        .with_capabilities(Arc::new(compute.clone()))
        .with_executor(Arc::new(compute))
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()));
    let mut spec = WorkSpec::new(WorkId::new("recovery"), WorkGoal::new(GOAL)).with_limits(limits);
    if require {
        spec = spec.with_required_output(HASH_EXPECTED_SHA256);
    }
    let report = agent
        .run_work(&spec, &ReportObservation, &ModelDecisionBoundary)
        .await;
    Some(Run {
        report,
        script,
        agent,
        limits,
    })
}

const LIMITS: WorkLimits = WorkLimits {
    max_turns: 4,
    max_executions: 2,
};

async fn run(replies: &[String]) -> Option<Run> {
    run_with(replies, LIMITS, true).await
}

/// The events that matter to the question, in order.
fn shape(report: &WorkReport) -> Vec<String> {
    report
        .events
        .iter()
        .filter_map(|e| {
            Some(match e {
                WorkEvent::DecisionStarted { .. } => "DecisionStarted".into(),
                WorkEvent::LocalDecision { decision, .. } => format!("LocalDecision:{decision}"),
                WorkEvent::ModelEscalation { .. } => "ModelEscalation".into(),
                WorkEvent::ModelCalled { .. } => "ModelCalled".into(),
                WorkEvent::DecisionMade { decision, .. } => format!("DecisionMade:{decision}"),
                WorkEvent::CapabilityRequested { capability, .. } => {
                    format!("CapabilityRequested:{capability}")
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
                WorkEvent::Execution(_) => "Execution?".into(),
                WorkEvent::ObservationRecorded { .. } => "ObservationRecorded".into(),
                WorkEvent::EvidenceRecorded { .. } => "EvidenceRecorded".into(),
                WorkEvent::EvidenceReused { .. } => "EvidenceReused".into(),
                WorkEvent::GoalEvaluated { satisfied, .. } => {
                    format!(
                        "GoalEvaluated:{}",
                        if *satisfied {
                            "satisfied"
                        } else {
                            "unsatisfied"
                        }
                    )
                }
                WorkEvent::WorkCompleted { .. } => "WorkCompleted".into(),
                WorkEvent::WorkBlocked { .. } => "WorkBlocked".into(),
                WorkEvent::WorkLimitReached { .. } => "WorkLimitReached".into(),
                WorkEvent::WorkFailed { .. } => "WorkFailed".into(),
                _ => return None,
            })
        })
        .collect()
}

fn count(report: &WorkReport, name: &str) -> usize {
    shape(report).iter().filter(|s| s.as_str() == name).count()
}

fn evaluations(report: &WorkReport) -> Vec<bool> {
    report
        .events
        .iter()
        .filter_map(|e| match e {
            WorkEvent::GoalEvaluated { satisfied, .. } => Some(*satisfied),
            _ => None,
        })
        .collect()
}

fn completed(report: &WorkReport) -> bool {
    matches!(report.outcome, WorkOutcome::Completed { .. })
}

/// The one rule every run must obey: completion only after an observation satisfied the goal.
fn no_completion_without_satisfaction(r: &Run, name: &str) {
    if completed(&r.report) {
        assert!(
            evaluations(&r.report).contains(&true),
            "{name}: completed without a satisfying observation"
        );
    }
    let shape = shape(&r.report);
    let first_ok = shape.iter().position(|s| s == "GoalEvaluated:satisfied");
    if let Some(done) = shape.iter().position(|s| s == "WorkCompleted") {
        assert!(
            first_ok.is_some_and(|ok| ok < done),
            "{name}: WorkCompleted before satisfaction"
        );
    }
    // Evidence only ever follows an observation; observations only follow executions.
    assert!(
        verify_trajectory(&r.report.events, &r.limits).is_empty(),
        "{name}: {:?}",
        verify_trajectory(&r.report.events, &r.limits)
    );
}

// ---- 1. Case A: the correct first decision ---------------------------------------------------

#[tokio::test]
async fn a_correct_execution_satisfies_the_goal_and_completes() {
    let Some(r) = run(&[request(RIGHT)]).await else {
        return;
    };
    assert_eq!(
        r.report.outcome.terminal_state(),
        chip_core::TerminalState::Completed
    );
    let m = r.report.measurement();
    assert_eq!(
        (m.model_calls, m.executions, m.observations, m.turns),
        (1, 1, 1, 2)
    );
    assert_eq!(count(&r.report, "EvidenceRecorded"), 1);
    assert_eq!(evaluations(&r.report), [true]);
    // Compute's own answer and receipt.
    assert_eq!(
        r.report.observations[0].output.as_deref(),
        Some(HASH_EXPECTED_SHA256)
    );
    assert!(
        r.report.observations[0]
            .receipt_id
            .as_deref()
            .is_some_and(|x| x.starts_with("sha256:"))
    );
    assert_eq!(
        shape(&r.report),
        [
            "DecisionStarted",
            "ModelEscalation",
            "ModelCalled",
            "DecisionMade:request compute.op_a",
            "CapabilityRequested:compute.op_a",
            "ExecutionRequested",
            "ExecutionStarted",
            "ExecutionCompleted",
            "ObservationRecorded",
            "EvidenceRecorded",
            "GoalEvaluated:satisfied",
            "DecisionStarted",
            "LocalDecision:complete",
            "DecisionMade:complete",
            "WorkCompleted"
        ]
    );
    no_completion_without_satisfaction(&r, "case A");
}

// ---- 2/3/6/7/8. Case B and C: a wrong, valid, real execution, then recovery -------------------

#[tokio::test]
async fn a_wrong_execution_is_real_and_does_not_satisfy_the_goal() {
    for wrong in [RUNTIME_INFO, SELF_TEST] {
        let Some(r) = run(&[request(wrong), request(RIGHT)]).await else {
            return;
        };
        // The first execution is real: Compute's output, Compute's receipt, recorded as evidence.
        let first = &r.report.observations[0];
        assert_eq!(first.kind, ObservationKind::ExecutionCompleted, "{wrong}");
        assert!(
            first
                .receipt_id
                .as_deref()
                .is_some_and(|x| x.starts_with("sha256:")),
            "{wrong}"
        );
        assert_ne!(
            first.output.as_deref(),
            Some(HASH_EXPECTED_SHA256),
            "{wrong}"
        );
        assert!(!first.output.as_deref().unwrap_or("").is_empty(), "{wrong}");
        assert_eq!(
            evaluations(&r.report)[0],
            false,
            "{wrong}: execution success is not goal satisfaction"
        );
        assert_eq!(count(&r.report, "EvidenceRecorded"), 2, "{wrong}");
    }
}

#[tokio::test]
async fn a_successful_wrong_execution_is_not_completed_work() {
    // After the wrong execution the local policy proposes to complete, as it would after any
    // successful execution. It is refused before it is even recorded as a decision.
    let Some(r) = run(&[request(RUNTIME_INFO), request(RIGHT)]).await else {
        return;
    };
    let shape = shape(&r.report);
    let wrong_eval = shape
        .iter()
        .position(|s| s == "GoalEvaluated:unsatisfied")
        .unwrap();
    assert!(!shape[..wrong_eval].contains(&"WorkCompleted".to_string()));
    // The turn after the wrong execution is an ordinary escalation, not a local completion.
    assert_eq!(
        &shape[wrong_eval + 1..wrong_eval + 3],
        ["DecisionStarted", "ModelEscalation"]
    );
    assert_eq!(
        count(&r.report, "LocalDecision:complete"),
        1,
        "only the post-recovery completion is a local decision"
    );
    let reason = r.report.events.iter().find_map(|e| match e {
        WorkEvent::ModelEscalation { reason, .. } if reason.contains("refused") => {
            Some(reason.clone())
        }
        _ => None,
    });
    assert!(reason.is_some(), "the refusal is visible in the trajectory");
}

#[tokio::test]
async fn a_wrong_first_decision_triggers_one_bounded_recovery_that_completes() {
    for wrong in [RUNTIME_INFO, SELF_TEST] {
        let Some(r) = run(&[request(wrong), request(RIGHT)]).await else {
            return;
        };
        assert!(completed(&r.report), "{wrong}: {:?}", r.report.outcome);
        let m = r.report.measurement();
        assert_eq!(
            (
                m.model_calls,
                m.model_escalations,
                m.executions,
                m.observations,
                m.local_decisions,
                m.turns
            ),
            (2, 2, 2, 2, 1, 3),
            "{wrong}"
        );
        assert_eq!(evaluations(&r.report), [false, true], "{wrong}");
        // The second observation is the digest, from Compute, with its own receipt.
        assert_eq!(
            r.report.observations[1].output.as_deref(),
            Some(HASH_EXPECTED_SHA256)
        );
        let receipts: Vec<_> = r
            .report
            .observations
            .iter()
            .map(|o| o.receipt_id.clone().unwrap())
            .collect();
        assert_ne!(receipts[0], receipts[1], "two executions, two receipts");
        assert!(
            matches!(&r.report.outcome, WorkOutcome::Completed { summary } if summary.contains(HASH_EXPECTED_SHA256))
        );
        // Exactly the trajectory shape, deterministic from run to run.
        assert_eq!(
            shape(&r.report),
            [
                "DecisionStarted",
                "ModelEscalation",
                "ModelCalled",
                &format!("DecisionMade:request {wrong}"),
                &format!("CapabilityRequested:{wrong}"),
                "ExecutionRequested",
                "ExecutionStarted",
                "ExecutionCompleted",
                "ObservationRecorded",
                "EvidenceRecorded",
                "GoalEvaluated:unsatisfied",
                "DecisionStarted",
                "ModelEscalation",
                "ModelCalled",
                "DecisionMade:request compute.op_a",
                "CapabilityRequested:compute.op_a",
                "ExecutionRequested",
                "ExecutionStarted",
                "ExecutionCompleted",
                "ObservationRecorded",
                "EvidenceRecorded",
                "GoalEvaluated:satisfied",
                "DecisionStarted",
                "LocalDecision:complete",
                "DecisionMade:complete",
                "WorkCompleted"
            ]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
            "{wrong}"
        );
        no_completion_without_satisfaction(&r, wrong);
    }
}

#[tokio::test]
async fn the_recovery_question_carries_the_authoritative_observation_and_no_recommendation() {
    let Some(r) = run(&[request(RUNTIME_INFO), request(RIGHT)]).await else {
        return;
    };
    let sent = r.script.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    let second = &sent[1];
    let first_output = r.report.observations[0].output.clone().unwrap();
    // Reality, as Compute observed it, and what Chip made of it.
    assert!(
        second.contains("execution.completed"),
        "the observation travels to the model: {second}"
    );
    assert!(
        second.contains(&first_output),
        "its real output is in the context"
    );
    assert!(
        second.contains(
            "capability compute.op_b: executed, but its observation did not satisfy the goal"
        ),
        "{second}"
    );
    assert!(
        second.contains("no authoritative observation satisfies the goal yet"),
        "{second}"
    );
    assert!(
        second.contains("request compute.op_b"),
        "the prior decision is listed: {second}"
    );
    // No answer is given: not the required value, and no capability is recommended.
    assert!(
        !second.contains(HASH_EXPECTED_SHA256),
        "the required output must not be revealed"
    );
    let after_ruled_out = second.split("Ruled out:").nth(1).unwrap();
    let ruled_and_evidence = format!(
        "{}{}",
        second
            .split("Evidence:")
            .nth(1)
            .unwrap()
            .split("Prior decisions:")
            .next()
            .unwrap(),
        after_ruled_out.split("Question:").next().unwrap()
    );
    assert!(
        !ruled_and_evidence.contains("compute.op_a")
            && !ruled_and_evidence.contains("compute.op_c"),
        "{ruled_and_evidence}"
    );
    for word in ["should", "recommend", "correct capability", "try "] {
        assert!(!ruled_and_evidence.to_lowercase().contains(word), "{word}");
    }
}

// ---- 4/5. Case E: the model claims success -------------------------------------------------

#[tokio::test]
async fn a_completion_claim_cannot_override_an_unsatisfied_observation() {
    // The claim even contains the right digest. It is the model's words, not an observation.
    let claim = format!(
        r#"{{"decision":"complete","summary":"Done: the digest is {HASH_EXPECTED_SHA256}"}}"#
    );
    for wrong in [RUNTIME_INFO, SELF_TEST] {
        let Some(r) = run(&[request(wrong), claim.clone()]).await else {
            return;
        };
        assert!(!completed(&r.report), "{wrong}");
        assert!(
            matches!(&r.report.outcome, WorkOutcome::Blocked { reason } if reason.starts_with("completion refused")),
            "{:?}",
            r.report.outcome
        );
        assert_eq!(count(&r.report, "WorkCompleted"), 0);
        // The wrong execution stays real, recorded, and unsatisfying; the claim added nothing.
        assert_eq!(r.report.measurement().executions, 1);
        assert_eq!(r.report.observations.len(), 1);
        assert_eq!(evaluations(&r.report), [false]);
        assert_eq!(
            r.script.sent.lock().unwrap().len(),
            2,
            "no further model call after the refused claim"
        );
        no_completion_without_satisfaction(&r, wrong);
    }
}

#[tokio::test]
async fn a_completion_claim_with_no_evidence_at_all_is_refused() {
    let claim =
        format!(r#"{{"decision":"complete","summary":"The digest is {HASH_EXPECTED_SHA256}"}}"#);
    let Some(r) = run(&[claim]).await else { return };
    assert!(!completed(&r.report));
    assert!(
        matches!(&r.report.outcome, WorkOutcome::Blocked { reason } if reason.starts_with("completion refused"))
    );
    // Nothing ran, nothing was observed, nothing was evaluated, nothing was recorded.
    let m = r.report.measurement();
    assert_eq!((m.executions, m.observations), (0, 0));
    assert!(evaluations(&r.report).is_empty());
    assert_eq!(count(&r.report, "EvidenceRecorded"), 0);
    assert_eq!(count(&r.report, "ExecutionStarted"), 0);
    no_completion_without_satisfaction(&r, "claim without evidence");
}

// ---- 9. Case D: wrong again ------------------------------------------------------------------

#[tokio::test]
async fn a_second_wrong_decision_does_not_complete_and_stays_bounded() {
    // Wrong, then the other wrong: two real executions, neither satisfying. The third decision is
    // a claim, which is refused. Nothing here ever completes.
    let Some(r) = run(&[
        request(RUNTIME_INFO),
        request(SELF_TEST),
        r#"{"decision":"complete","summary":"ok"}"#.into(),
    ])
    .await
    else {
        return;
    };
    assert!(!completed(&r.report));
    assert_eq!(evaluations(&r.report), [false, false]);
    assert_eq!(r.report.measurement().executions, 2);
    assert!(
        matches!(&r.report.outcome, WorkOutcome::Blocked { reason } if reason.starts_with("completion refused"))
    );
    no_completion_without_satisfaction(&r, "wrong twice, then a claim");

    // Wrong, wrong, and then the model has nothing more to say: the run fails closed.
    let Some(r) = run(&[request(RUNTIME_INFO), request(SELF_TEST)]).await else {
        return;
    };
    assert!(!completed(&r.report));
    assert!(matches!(r.report.outcome, WorkOutcome::Failed { .. }));
    no_completion_without_satisfaction(&r, "wrong twice, then silence");
}

#[tokio::test]
async fn asking_for_the_wrong_capability_again_reuses_evidence_and_stays_bounded() {
    // The same wrong capability over and over: the first execution is real, the rest reuse its
    // evidence (nothing re-runs), none satisfies the goal, and the turn limit ends it.
    let same = request(RUNTIME_INFO);
    let Some(r) = run(&[same.clone(), same.clone(), same.clone(), same]).await else {
        return;
    };
    assert!(!completed(&r.report));
    assert!(
        matches!(
            r.report.outcome,
            WorkOutcome::LimitReached {
                limit: LimitKind::Turns
            }
        ),
        "{:?}",
        r.report.outcome
    );
    assert_eq!(r.report.measurement().executions, 1);
    assert!(count(&r.report, "EvidenceReused") >= 1);
    assert!(evaluations(&r.report).iter().all(|s| !s));
    no_completion_without_satisfaction(&r, "wrong repeatedly");
}

// ---- 10/11. Limits ----------------------------------------------------------------------------

#[tokio::test]
async fn recovery_respects_the_turn_limit() {
    // Wrong then right needs three turns (the third is the completion). With two, the goal is
    // satisfied by evidence but the work is not completed: it stops at the limit.
    let limits = WorkLimits {
        max_turns: 2,
        max_executions: 2,
    };
    let Some(r) = run_with(&[request(RUNTIME_INFO), request(RIGHT)], limits, true).await else {
        return;
    };
    assert!(
        matches!(
            r.report.outcome,
            WorkOutcome::LimitReached {
                limit: LimitKind::Turns
            }
        ),
        "{:?}",
        r.report.outcome
    );
    assert_eq!(r.report.measurement().turns, 2);
    assert_eq!(evaluations(&r.report), [false, true]);
    assert_eq!(count(&r.report, "WorkCompleted"), 0);
    no_completion_without_satisfaction(&r, "turn limit");
}

#[tokio::test]
async fn recovery_respects_the_execution_limit() {
    let limits = WorkLimits {
        max_turns: 4,
        max_executions: 1,
    };
    let Some(r) = run_with(&[request(RUNTIME_INFO), request(RIGHT)], limits, true).await else {
        return;
    };
    assert!(
        matches!(
            r.report.outcome,
            WorkOutcome::LimitReached {
                limit: LimitKind::Executions
            }
        ),
        "{:?}",
        r.report.outcome
    );
    // The recovery was requested and refused by the budget: Compute ran exactly once.
    assert_eq!(count(&r.report, "ExecutionStarted"), 1);
    assert_eq!(r.report.observations.len(), 1);
    assert_eq!(evaluations(&r.report), [false]);
    assert!(!completed(&r.report));
    no_completion_without_satisfaction(&r, "execution limit");
}

// ---- 12. Invalid recovery decisions execute nothing -----------------------------------------

#[tokio::test]
async fn an_invalid_recovery_decision_executes_nothing() {
    let invented = format!(
        r#"{{"decision":"request_capability","capability":"{RIGHT}","inputs":{{"text":"the fixed test input"}}}}"#
    );
    for second in [
        invented,
        r#"{"decision":"request_capability","capability":"compute.op_fake"}"#.to_string(),
        "I will pick the digest one.".to_string(),
    ] {
        let Some(r) = run(&[request(RUNTIME_INFO), second.clone()]).await else {
            return;
        };
        assert!(!completed(&r.report), "{second}");
        // Only the wrong execution happened; the invalid request produced nothing.
        assert_eq!(count(&r.report, "ExecutionStarted"), 1, "{second}");
        assert_eq!(r.report.observations.len(), 1, "{second}");
        assert_eq!(count(&r.report, "EvidenceRecorded"), 1, "{second}");
        assert_eq!(evaluations(&r.report), [false], "{second}");
        no_completion_without_satisfaction(&r, &second);
    }
}

// ---- adversarial: receipts and evidence are not goal satisfaction -----------------------------

#[tokio::test]
async fn a_receipt_proves_execution_not_goal_satisfaction() {
    let Some(r) = run(&[request(SELF_TEST), request(RIGHT)]).await else {
        return;
    };
    let wrong = &r.report.observations[0];
    // A real receipt, from Compute...
    assert!(
        wrong
            .receipt_id
            .as_deref()
            .is_some_and(|x| x.starts_with("sha256:") && x.len() > 20)
    );
    assert_eq!(wrong.kind, ObservationKind::ExecutionCompleted);
    // ...which the goal evaluator did not accept.
    assert_eq!(evaluations(&r.report)[0], false);
}

#[tokio::test]
async fn evidence_exists_for_the_wrong_execution_and_the_goal_stays_unsatisfied() {
    let Some(r) = run(&[
        request(RUNTIME_INFO),
        r#"{"decision":"complete","summary":"done"}"#.into(),
    ])
    .await
    else {
        return;
    };
    let probe = CapabilityRequest::new(
        ExecutionId::new("probe"),
        CapabilityId::new(RUNTIME_INFO).unwrap(),
    );
    // Evidence tells us what happened...
    assert!(
        matches!(r.agent.lookup_evidence(&probe), EvidenceLookup::Found(o) if o.output != Some(HASH_EXPECTED_SHA256.into()))
    );
    // ...and says nothing about the digest capability, which never ran.
    let digest =
        CapabilityRequest::new(ExecutionId::new("probe"), CapabilityId::new(RIGHT).unwrap());
    assert_eq!(r.agent.lookup_evidence(&digest), EvidenceLookup::NotFound);
    assert!(!completed(&r.report));
}

// ---- 13-16. Properties over every script -----------------------------------------------------

#[tokio::test]
async fn no_script_completes_without_satisfying_evidence_and_trajectories_are_deterministic() {
    let claim = r#"{"decision":"complete","summary":"done"}"#.to_string();
    let scripts: Vec<Vec<String>> = vec![
        vec![request(RIGHT)],
        vec![request(RUNTIME_INFO), request(RIGHT)],
        vec![request(SELF_TEST), request(RIGHT)],
        vec![request(RUNTIME_INFO), request(SELF_TEST), claim.clone()],
        vec![request(RUNTIME_INFO), claim.clone()],
        vec![claim.clone()],
        vec![
            request(SELF_TEST),
            request(SELF_TEST),
            request(SELF_TEST),
            request(SELF_TEST),
        ],
        vec![request(RUNTIME_INFO), "not json".into()],
    ];
    for (i, replies) in scripts.iter().enumerate() {
        let (Some(a), Some(b)) = (run(replies).await, run(replies).await) else {
            return;
        };
        let name = format!("script {i}");
        no_completion_without_satisfaction(&a, &name);
        assert_eq!(shape(&a.report), shape(&b.report), "{name}: deterministic");
        assert_eq!(
            a.report.outcome.terminal_state(),
            b.report.outcome.terminal_state(),
            "{name}"
        );
        // Every observation has an execution behind it and every piece of evidence an observation.
        let m = a.report.measurement();
        assert!(m.observations <= m.executions, "{name}");
        assert_eq!(
            count(&a.report, "ObservationRecorded") as u32,
            m.observations,
            "{name}"
        );
        assert_eq!(
            count(&a.report, "ExecutionStarted") as u32,
            m.executions,
            "{name}: executions count ExecutionStarted"
        );
    }
}

#[tokio::test]
async fn without_a_requirement_nothing_changes() {
    // The evaluator is opt-in: with no required output no GoalEvaluated event exists and an
    // execution that succeeds still completes the work, exactly as before PR38.
    let Some(r) = run_with(&[request(RUNTIME_INFO)], LIMITS, false).await else {
        return;
    };
    assert!(completed(&r.report));
    assert!(evaluations(&r.report).is_empty());
    assert_eq!(
        count(&r.report, "GoalEvaluated:satisfied") + count(&r.report, "GoalEvaluated:unsatisfied"),
        0
    );
}
