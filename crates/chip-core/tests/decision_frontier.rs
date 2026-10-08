//! The Decision Frontier: what a piece of work has left unresolved, kept by the runtime from
//! authoritative observations alone.
//!
//! Every run below is real: the loop, the validation, the execution boundary, the observation and the
//! safety audit. Only the model's decisions are scripted (a policy that plays back a fixed list) and the
//! executor's results are fixed, so each case is exact. Nothing a script says is evidence: a frontier
//! item moves only when an observation the executor produced satisfies its predicate.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, CapabilityRequest, EvidenceState, ExecutionError, ExecutionId,
    ExecutionObserver, ExecutionRequest, ExecutionResult, ExecutionStatus, Executor,
    FrontierItemSpec, FrontierKind, FrontierStatus, LocalReasoningResult, Observation,
    ObservationPredicate, ScriptedPolicy, TestLocalReasoner, WorkDecision, WorkDecisionBoundary,
    WorkEvent, WorkGoal, WorkId, WorkOutcome, WorkReport, WorkSpec, audit_safety, measure_utility,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

// ---- the harness ------------------------------------------------------------------------------------

struct Fx;

#[async_trait::async_trait]
impl ModelProvider for Fx {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        Ok(ModelResponse::new("r", "unused", Usage::new(1, 1)))
    }
}

/// Executes whatever is asked, returning the next fixed result in order.
struct Exec(Mutex<VecDeque<(ExecutionStatus, &'static str)>>);

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        let (status, output) = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("the script ran out of results");
        Ok(ExecutionResult {
            id: r.id,
            status,
            output: output.to_string(),
            receipt_id: None,
        })
    }
}

struct Caps;

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        ["look.list", "look.read", "act.write", "check.run"]
            .iter()
            .map(|id| {
                Ok(CapabilityDescriptor::new(CapabilityId::new(*id)?, *id, "x")
                    .without_evidence_reuse())
            })
            .collect()
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

struct NoModel;

impl WorkDecisionBoundary for NoModel {
    fn interpret(
        &self,
        _r: &ModelResponse,
        _c: &[chip_core::Capability],
    ) -> Result<WorkDecision, chip_core::DecisionError> {
        Err(chip_core::DecisionError::InvalidDecision("no model".into()))
    }
}

fn ask(capability: &str, id: &str) -> Option<WorkDecision> {
    Some(WorkDecision::RequestCapability(CapabilityRequest::new(
        ExecutionId::new(id),
        CapabilityId::new(capability).unwrap(),
    )))
}

fn done() -> Option<WorkDecision> {
    Some(WorkDecision::Complete {
        summary: "done".into(),
    })
}

const OK: ExecutionStatus = ExecutionStatus::Success;
const FAIL: ExecutionStatus = ExecutionStatus::Failure;

async fn go(
    spec: &WorkSpec,
    decisions: Vec<Option<WorkDecision>>,
    results: Vec<(ExecutionStatus, &'static str)>,
) -> WorkReport {
    let reasoner = Arc::new(
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
    );
    let agent = Agent::new(Arc::new(Fx))
        .with_capabilities(Arc::new(Caps))
        .with_executor(Arc::new(Exec(Mutex::new(results.into()))))
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(reasoner);
    agent
        .run_work(spec, &ScriptedPolicy::new(decisions), &NoModel)
        .await
}

fn declared() -> Vec<CapabilityId> {
    ["look.list", "look.read", "act.write", "check.run"]
        .iter()
        .map(|id| CapabilityId::new(*id).unwrap())
        .collect()
}

// ---- the predicates (the work's own definitions of "answered") --------------------------------------

/// Answered when a successful observation's output is exactly the marker.
#[derive(Debug)]
struct Saw(&'static str);

impl ObservationPredicate for Saw {
    fn describe(&self) -> String {
        format!("{} was observed", self.0)
    }
    fn satisfied_by(&self, o: &Observation) -> bool {
        o.status == ExecutionStatus::Success && o.output.as_deref() == Some(self.0)
    }
}

/// Answered when a successful observation's output starts with the marker (a write says what it wrote).
#[derive(Debug)]
struct SawPrefix(&'static str);

impl ObservationPredicate for SawPrefix {
    fn describe(&self) -> String {
        format!("{}* was observed", self.0)
    }
    fn satisfied_by(&self, o: &Observation) -> bool {
        o.status == ExecutionStatus::Success
            && o.output.as_deref().is_some_and(|t| t.starts_with(self.0))
    }
}

/// Answered when a pass was observed after the last write. A later write takes it back.
#[derive(Debug)]
struct PassedAfterWrite;

impl ObservationPredicate for PassedAfterWrite {
    fn describe(&self) -> String {
        "a pass was observed after the last write".into()
    }
    fn satisfied_by(&self, _o: &Observation) -> bool {
        false
    }
    fn satisfied_by_trajectory(&self, observations: &[Observation]) -> bool {
        let at = |marker: &str, ok: bool| {
            observations.iter().rposition(|o| {
                o.output.as_deref().is_some_and(|t| t.starts_with(marker))
                    && (o.status == ExecutionStatus::Success) == ok
            })
        };
        match (at("W", true), at("PASS", true)) {
            (Some(w), Some(p)) => p > w,
            _ => false,
        }
    }
}

/// The frontier a software change starts with: seen, changed, verified.
fn change_spec() -> WorkSpec {
    WorkSpec::new(WorkId::new("w"), WorkGoal::new("change it and verify it"))
        .with_frontier_item(FrontierItemSpec::new(
            FrontierKind::MissingEvidence,
            "Has the project been listed?",
            Arc::new(Saw("L")),
        ))
        .with_frontier_item(FrontierItemSpec::new(
            FrontierKind::MissingEvidence,
            "Has the change been made?",
            Arc::new(SawPrefix("W")),
        ))
        .with_frontier_item(FrontierItemSpec::new(
            FrontierKind::UnverifiedHypothesis,
            "Does the changed project pass?",
            Arc::new(PassedAfterWrite),
        ))
        .with_required_observation(Arc::new(PassedAfterWrite))
}

fn status_of(report: &WorkReport) -> Vec<(String, &'static str)> {
    report
        .frontier
        .items()
        .iter()
        .map(|i| (i.id.to_string(), i.status.name()))
        .collect()
}

fn frontier_events(report: &WorkReport) -> Vec<String> {
    report
        .events
        .iter()
        .filter_map(|e| match e {
            WorkEvent::FrontierOpened { item, turn, .. } => Some(format!("opened {item} @{turn}")),
            WorkEvent::FrontierResolved { item, evidence, .. } => {
                Some(format!("resolved {item} by {evidence}"))
            }
            WorkEvent::FrontierInvalidated { item, evidence, .. } => {
                Some(format!("invalidated {item} by {evidence}"))
            }
            WorkEvent::FrontierProgress {
                evidence, resolved, ..
            } => Some(format!("progress {evidence} {resolved}")),
            _ => None,
        })
        .collect()
}

/// What the old accounting would have said: every executed turn whose goal evaluation was false.
fn legacy_misses(report: &WorkReport) -> usize {
    report
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                WorkEvent::GoalEvaluated {
                    satisfied: false,
                    ..
                }
            )
        })
        .count()
}

// ---- initialization ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_declared_frontier_is_opened_first_with_runtime_identities() {
    let report = go(&change_spec(), vec![done()], vec![]).await;
    assert_eq!(
        frontier_events(&report),
        ["opened F1 @0", "opened F2 @0", "opened F3 @0"]
    );
    let items = report.frontier.items();
    assert_eq!(
        items.iter().map(|i| i.id.to_string()).collect::<Vec<_>>(),
        ["F1", "F2", "F3"]
    );
    assert!(
        items
            .iter()
            .all(|i| i.status == FrontierStatus::Open && i.resolution.is_none())
    );
    assert_eq!(items[0].kind, FrontierKind::MissingEvidence);
    assert_eq!(items[2].kind, FrontierKind::UnverifiedHypothesis);
    assert_eq!(items[1].question, "Has the change been made?");
    // The frontier opens before anything is decided.
    let first_decision = report
        .events
        .iter()
        .position(|e| matches!(e, WorkEvent::DecisionStarted { .. }))
        .unwrap();
    let last_open = report
        .events
        .iter()
        .rposition(|e| matches!(e, WorkEvent::FrontierOpened { .. }))
        .unwrap();
    assert!(last_open < first_decision);
}

#[tokio::test]
async fn an_undeclared_frontier_is_derived_from_the_requirements_and_nothing_else() {
    let none = WorkSpec::new(WorkId::new("w"), WorkGoal::new("g"));
    let report = go(&none, vec![done()], vec![]).await;
    assert!(
        report.frontier.items().is_empty(),
        "no requirement, no frontier"
    );
    assert!(frontier_events(&report).is_empty());

    let derived = WorkSpec::new(WorkId::new("w"), WorkGoal::new("g"))
        .with_required_output("X")
        .with_required_observation(Arc::new(PassedAfterWrite));
    let report = go(&derived, vec![done()], vec![]).await;
    let questions: Vec<_> = report
        .frontier
        .items()
        .iter()
        .map(|i| i.question.clone())
        .collect();
    assert_eq!(
        questions,
        [
            "Has the required output \"X\" been produced?".to_string(),
            "a pass was observed after the last write".to_string()
        ]
    );
}

// ---- resolution -----------------------------------------------------------------------------------------

#[tokio::test]
async fn an_observation_that_establishes_the_question_resolves_it_with_its_evidence() {
    let report = go(
        &change_spec(),
        vec![ask("look.list", "e1"), done()],
        vec![(OK, "L")],
    )
    .await;
    assert_eq!(
        status_of(&report),
        [
            ("F1".into(), "resolved"),
            ("F2".into(), "open"),
            ("F3".into(), "open")
        ]
    );
    let f1 = report.frontier.items()[0].resolution.clone().unwrap();
    assert_eq!(
        f1.evidence,
        ExecutionId::new("e1"),
        "the resolution names the execution"
    );
    assert!(frontier_events(&report).contains(&"resolved F1 by e1".to_string()));
    assert!(frontier_events(&report).contains(&"progress e1 1".to_string()));
}

#[tokio::test]
async fn an_unrelated_observation_leaves_the_question_open() {
    let report = go(
        &change_spec(),
        vec![ask("look.read", "e1"), done()],
        vec![(OK, "R")],
    )
    .await;
    assert!(
        report
            .frontier
            .items()
            .iter()
            .all(|i| i.status == FrontierStatus::Open)
    );
    assert!(
        !frontier_events(&report)
            .iter()
            .any(|e| e.starts_with("resolved"))
    );
}

#[tokio::test]
async fn partial_evidence_does_not_answer_a_question_it_does_not_answer() {
    // "L-partial" is an observation of the listing, and not the one the question asks for.
    let report = go(
        &change_spec(),
        vec![ask("look.list", "e1"), done()],
        vec![(OK, "L-partial")],
    )
    .await;
    assert_eq!(report.frontier.items()[0].status, FrontierStatus::Open);
    // And a failed execution never resolves anything, whatever its output says.
    let report = go(
        &change_spec(),
        vec![ask("look.list", "e1"), done()],
        vec![(FAIL, "L")],
    )
    .await;
    assert_eq!(report.frontier.items()[0].status, FrontierStatus::Open);
}

#[tokio::test]
async fn a_write_that_succeeds_resolves_the_change_and_leaves_verification_open() {
    let report = go(
        &change_spec(),
        vec![ask("act.write", "e1"), done()],
        vec![(OK, "W")],
    )
    .await;
    assert_eq!(
        status_of(&report),
        [
            ("F1".into(), "open"),
            ("F2".into(), "resolved"),
            ("F3".into(), "open")
        ]
    );
}

#[tokio::test]
async fn verification_resolves_the_verification_question_and_only_it() {
    let report = go(
        &change_spec(),
        vec![ask("act.write", "e1"), ask("check.run", "e2"), done()],
        vec![(OK, "W"), (OK, "PASS")],
    )
    .await;
    assert_eq!(report.frontier.items()[2].status, FrontierStatus::Resolved);
    assert_eq!(
        report.frontier.items()[2]
            .resolution
            .as_ref()
            .unwrap()
            .evidence,
        ExecutionId::new("e2")
    );
    assert_eq!(
        report.frontier.items()[0].status,
        FrontierStatus::Open,
        "nothing listed it"
    );
    assert!(matches!(report.outcome, WorkOutcome::Completed { .. }));
}

#[tokio::test]
async fn a_failed_verification_leaves_the_question_open_and_opens_what_the_failure_raised() {
    let report = go(
        &change_spec(),
        vec![ask("act.write", "e1"), ask("check.run", "e2"), done()],
        vec![(OK, "W"), (FAIL, "FAIL")],
    )
    .await;
    assert_eq!(report.frontier.items()[2].status, FrontierStatus::Open);
    // The failure is evidence on the record, and it changed the frontier.
    let failed = report.observations.last().unwrap();
    assert_eq!(failed.status, ExecutionStatus::Failure);
    assert_eq!(failed.output.as_deref(), Some("FAIL"));
    let raised = report.frontier.items().last().unwrap();
    assert_eq!(raised.id.to_string(), "F4");
    assert_eq!(raised.status, FrontierStatus::Open);
    assert!(
        raised
            .question
            .contains("check.run succeed after execution e2 failed")
    );
    // Not completed: the goal's own evaluation decides that, and it is unmet.
    assert!(!matches!(report.outcome, WorkOutcome::Completed { .. }));
}

#[tokio::test]
async fn a_failure_is_answered_by_a_later_success_of_the_same_capability() {
    let report = go(
        &change_spec(),
        vec![
            ask("act.write", "e1"),
            ask("check.run", "e2"),
            ask("check.run", "e3"),
            done(),
        ],
        vec![(OK, "W"), (FAIL, "FAIL"), (OK, "PASS")],
    )
    .await;
    let f4 = report
        .frontier
        .items()
        .iter()
        .find(|i| i.id.to_string() == "F4")
        .unwrap();
    assert_eq!(f4.status, FrontierStatus::Resolved);
    assert_eq!(
        f4.resolution.as_ref().unwrap().evidence,
        ExecutionId::new("e3")
    );
    // A second identical failure does not open a second question while one is open.
    let report = go(
        &change_spec(),
        vec![ask("check.run", "e1"), ask("check.run", "e2"), done()],
        vec![(FAIL, "FAIL"), (FAIL, "FAIL")],
    )
    .await;
    assert_eq!(
        report.frontier.items().len(),
        4,
        "F1..F3 and one failure question"
    );
}

// ---- invalidation ------------------------------------------------------------------------------------------

#[tokio::test]
async fn new_evidence_that_supersedes_an_answer_invalidates_it_and_opens_a_successor() {
    let report = go(
        &change_spec(),
        vec![
            ask("act.write", "e1"),
            ask("check.run", "e2"),
            ask("act.write", "e3"),
            done(),
        ],
        vec![(OK, "W1"), (OK, "PASS"), (OK, "W2")],
    )
    .await;
    let events = frontier_events(&report);
    assert!(
        events.contains(&"resolved F3 by e2".to_string()),
        "{events:?}"
    );
    assert!(
        events.contains(&"invalidated F3 by e3".to_string()),
        "{events:?}"
    );
    let items = report.frontier.items();
    assert_eq!(items[2].status, FrontierStatus::Invalidated);
    assert_eq!(
        items[2].resolution.as_ref().unwrap().evidence,
        ExecutionId::new("e3")
    );
    let successor = items.last().unwrap();
    assert_eq!(
        (successor.id.to_string(), successor.status),
        ("F4".to_string(), FrontierStatus::Open)
    );
    assert_eq!(
        successor.question, items[2].question,
        "the same question, open again"
    );
    // A stale pass does not stand.
    assert!(!matches!(report.outcome, WorkOutcome::Completed { .. }));
}

// ---- decision accounting -------------------------------------------------------------------------------------

#[tokio::test]
async fn legitimate_progress_is_not_a_wrong_decision_and_not_recovery() {
    let spec = change_spec();
    let report = go(
        &spec,
        vec![
            ask("look.list", "e1"),
            ask("look.read", "e2"),
            ask("act.write", "e3"),
            ask("check.run", "e4"),
            done(),
        ],
        vec![(OK, "L"), (OK, "R"), (OK, "W"), (OK, "PASS")],
    )
    .await;
    assert!(matches!(report.outcome, WorkOutcome::Completed { .. }));
    // The old reading: three misses, because the goal was unmet after each of the first three.
    assert_eq!(legacy_misses(&report), 3);
    let u = measure_utility(&report, &spec);
    assert_eq!(u.wrong_valid_decisions, 0, "no step was wrong");
    assert_eq!(
        (
            u.recovery_turns,
            u.recovery_executions,
            u.recovery_model_calls
        ),
        (0, 0, 0),
        "nothing was recovered from"
    );
    assert_eq!(
        u.supporting_decisions, 1,
        "the read added information and moved no item"
    );
    assert_eq!(
        u.frontier_progress_events, 3,
        "list, write and verification each answered a question"
    );
    assert_eq!(
        (
            u.frontier_opened,
            u.frontier_resolved,
            u.frontier_invalidated,
            u.frontier_remaining
        ),
        (3, 3, 0, 0)
    );
    assert_eq!(
        (u.verified_outputs, u.failed_observations, u.recoveries),
        (1, 0, 0)
    );
    audit_safety(&report, &spec, &declared()).assert_clean();
}

#[tokio::test]
async fn an_actual_failure_is_where_recovery_begins() {
    let spec = change_spec();
    let report = go(
        &spec,
        vec![
            ask("act.write", "e1"),
            ask("check.run", "e2"),
            ask("act.write", "e3"),
            ask("check.run", "e4"),
            done(),
        ],
        vec![(OK, "W1"), (FAIL, "FAIL"), (OK, "W2"), (OK, "PASS")],
    )
    .await;
    assert!(matches!(report.outcome, WorkOutcome::Completed { .. }));
    let u = measure_utility(&report, &spec);
    assert_eq!(u.failed_observations, 1);
    assert_eq!(u.recoveries, 1, "a failure followed by a further execution");
    assert_eq!(
        u.wrong_valid_decisions, 0,
        "the repair write is not a wrong decision"
    );
    assert_eq!(
        u.recovery_executions, 2,
        "the repair and the second verification followed the failure"
    );
    assert_eq!(u.supporting_decisions, 1, "the repair write");
    audit_safety(&report, &spec, &declared()).assert_clean();
}

#[tokio::test]
async fn invalidation_begins_recovery_without_a_failed_execution() {
    let spec = change_spec();
    let report = go(
        &spec,
        vec![
            ask("act.write", "e1"),
            ask("check.run", "e2"),
            ask("act.write", "e3"),
            ask("check.run", "e4"),
            done(),
        ],
        vec![(OK, "W1"), (OK, "PASS"), (OK, "W2"), (OK, "PASS")],
    )
    .await;
    let u = measure_utility(&report, &spec);
    assert_eq!(u.failed_observations, 0);
    assert_eq!(u.frontier_invalidated, 1);
    assert!(
        u.recovery_executions >= 1,
        "the work after the invalidation is recovery"
    );
}

#[tokio::test]
async fn an_execution_that_adds_nothing_is_a_wrong_valid_decision() {
    // Reading the same thing twice, identically: it told the work nothing new.
    let spec = change_spec();
    let report = go(
        &spec,
        vec![ask("look.read", "e1"), ask("look.read", "e2"), done()],
        vec![(OK, "R"), (OK, "R")],
    )
    .await;
    let u = measure_utility(&report, &spec);
    assert_eq!((u.supporting_decisions, u.wrong_valid_decisions), (1, 1));
    assert!(u.recovery_executions == 0 && u.recovery_turns >= 0);
}

#[tokio::test]
async fn work_whose_frontier_is_only_its_requirements_keeps_the_single_step_accounting() {
    // A required output is the whole question: an execution that does not produce it moved nothing.
    let spec = WorkSpec::new(WorkId::new("w"), WorkGoal::new("g")).with_required_output("X");
    let report = go(
        &spec,
        vec![ask("look.list", "e1"), ask("look.read", "e2"), done()],
        vec![(OK, "Y"), (OK, "X")],
    )
    .await;
    let u = measure_utility(&report, &spec);
    assert_eq!(
        u.wrong_valid_decisions, 1,
        "the first execution produced the wrong thing"
    );
    assert_eq!(
        u.supporting_decisions, 0,
        "single-step work has no support category"
    );
    assert_eq!(
        (u.frontier_opened, u.frontier_resolved, u.frontier_remaining),
        (1, 1, 0)
    );
    assert!(
        u.recovery_executions >= 1,
        "the work after the wrong decision is recovery"
    );
    assert!(matches!(report.outcome, WorkOutcome::Completed { .. }));
}

// ---- independence ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn resolving_the_frontier_is_not_completing_the_work() {
    // Every declared item is answered, and the goal's own requirement is not met: the model's claim
    // of completion is refused by the goal evaluation, which the frontier does not touch.
    let spec = WorkSpec::new(WorkId::new("w"), WorkGoal::new("g"))
        .with_frontier_item(FrontierItemSpec::new(
            FrontierKind::MissingEvidence,
            "Has the project been listed?",
            Arc::new(Saw("L")),
        ))
        .with_required_observation(Arc::new(PassedAfterWrite));
    let report = go(&spec, vec![ask("look.list", "e1"), done()], vec![(OK, "L")]).await;
    assert_eq!(
        report.frontier.remaining(),
        0,
        "the frontier is fully resolved"
    );
    assert!(
        !matches!(report.outcome, WorkOutcome::Completed { .. }),
        "a claim of completion is refused while the goal is unmet: {:?}",
        report.outcome
    );
    let u = measure_utility(&report, &spec);
    assert_eq!(u.verified_outputs, 0, "and nothing was verified");
    assert_eq!(u.work_per_model_call(), Some(0.0));
}

#[tokio::test]
async fn a_frontier_transition_must_rest_on_a_recorded_observation() {
    let spec = change_spec();
    let mut report = go(&spec, vec![ask("look.list", "e1"), done()], vec![(OK, "L")]).await;
    audit_safety(&report, &spec, &declared()).assert_clean();
    // A transition naming an execution that never produced an observation is a violation.
    let at = report
        .events
        .iter()
        .position(|e| matches!(e, WorkEvent::FrontierResolved { .. }))
        .unwrap();
    if let WorkEvent::FrontierResolved { evidence, .. } = &mut report.events[at] {
        *evidence = ExecutionId::new("invented");
    }
    let audit = audit_safety(&report, &spec, &declared());
    assert_eq!(audit.frontier_without_evidence, 1);
    assert!(!audit.is_clean());
}

#[tokio::test]
async fn the_frontier_is_work_local() {
    let spec = change_spec();
    let a = go(&spec, vec![ask("look.list", "e1"), done()], vec![(OK, "L")]).await;
    let b = go(&spec, vec![done()], vec![]).await;
    assert_eq!(a.frontier.resolved(), 1);
    assert_eq!(
        b.frontier.resolved(),
        0,
        "nothing carries from one work to the next"
    );
    assert_eq!(
        b.frontier.items()[0].id.to_string(),
        "F1",
        "identities restart with each work"
    );
}
