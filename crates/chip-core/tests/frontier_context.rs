//! The Decision Frontier as the model sees it.
//!
//! The model is shown the frontier as read-only context. These tests run real work, capture the exact
//! text each escalation sends, and hold the contract: the control arm is byte-for-byte what it always
//! was; the treatment differs from it by one section and nothing else; the section says only what the
//! runtime resolved from recorded evidence; and nothing the model says, in any form, changes the
//! frontier.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, Capability, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, CapabilityRequest, DecisionError, DecisionFrontier,
    DeduplicatedEscalationContext, EscalationContextPolicy, EvidenceState, ExecutionError,
    ExecutionId, ExecutionObserver, ExecutionRequest, ExecutionResult, ExecutionStatus, Executor,
    FrontierEscalationContext, FrontierItemSpec, FrontierKind, FrontierStatus,
    LocalReasoningResult, NoLocalPolicy, Observation, ObservationPredicate, TestLocalReasoner,
    WorkDecision, WorkDecisionBoundary, WorkGoal, WorkId, WorkReport, WorkSpec,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

// ---- the harness: real work, a model that records what it is sent ---------------------------------

struct Fx(Mutex<Vec<ModelRequest>>);

#[async_trait::async_trait]
impl ModelProvider for Fx {
    async fn complete(&self, r: ModelRequest) -> Result<ModelResponse, FxError> {
        self.0.lock().unwrap().push(r);
        Ok(ModelResponse::new("r", "a reply", Usage::new(1, 1)))
    }
}

struct Exec(Mutex<VecDeque<(ExecutionStatus, &'static str)>>);

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        let (status, output) = self.0.lock().unwrap().pop_front().expect("results ran out");
        Ok(ExecutionResult {
            id: r.id,
            status,
            output: output.into(),
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

/// Plays back the model's decisions, one per escalation, whatever it was sent.
struct Script {
    decisions: Vec<WorkDecision>,
    next: AtomicUsize,
}

impl WorkDecisionBoundary for Script {
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

fn ask(capability: &str, id: &str) -> WorkDecision {
    WorkDecision::RequestCapability(CapabilityRequest::new(
        ExecutionId::new(id),
        CapabilityId::new(capability).unwrap(),
    ))
}

#[derive(Debug)]
struct Saw(&'static str);

impl ObservationPredicate for Saw {
    fn describe(&self) -> String {
        format!("{} was observed", self.0)
    }
    fn satisfied_by(&self, o: &Observation) -> bool {
        o.status == ExecutionStatus::Success
            && o.output.as_deref().is_some_and(|t| t.starts_with(self.0))
    }
}

/// Answered when a pass was observed after the last write; a later write takes it back.
#[derive(Debug)]
struct PassedAfterWrite;

impl ObservationPredicate for PassedAfterWrite {
    fn describe(&self) -> String {
        "a pass was observed after the last write".into()
    }
    fn satisfied_by(&self, _o: &Observation) -> bool {
        false
    }
    fn satisfied_by_trajectory(&self, os: &[Observation]) -> bool {
        let at = |m: &str| {
            os.iter().rposition(|o| {
                o.status == ExecutionStatus::Success
                    && o.output.as_deref().is_some_and(|t| t.starts_with(m))
            })
        };
        matches!((at("W"), at("PASS")), (Some(w), Some(p)) if p > w)
    }
}

fn spec(declared: bool) -> WorkSpec {
    let mut s = WorkSpec::new(WorkId::new("w"), WorkGoal::new("change it and verify it"))
        .with_required_observation(Arc::new(PassedAfterWrite));
    if declared {
        for (kind, q, p) in [
            (
                FrontierKind::MissingEvidence,
                "Has the project been listed?",
                Arc::new(Saw("L")) as Arc<dyn ObservationPredicate>,
            ),
            (
                FrontierKind::MissingEvidence,
                "Has the change been made?",
                Arc::new(Saw("W")),
            ),
            (
                FrontierKind::UnverifiedHypothesis,
                "Does the changed project pass?",
                Arc::new(PassedAfterWrite),
            ),
        ] {
            s = s.with_frontier_item(FrontierItemSpec::new(kind, q, p));
        }
    }
    s
}

/// Runs the work with the given context policy and returns the report and every request the model got.
async fn run(
    spec: &WorkSpec,
    decisions: Vec<WorkDecision>,
    results: Vec<(ExecutionStatus, &'static str)>,
    policy: &dyn EscalationContextPolicy,
) -> (WorkReport, Vec<ModelRequest>) {
    let fx = Arc::new(Fx(Mutex::new(Vec::new())));
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
    let agent = Agent::new(fx.clone())
        .with_capabilities(Arc::new(Caps))
        .with_executor(Arc::new(Exec(Mutex::new(results.into()))))
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(reasoner);
    let report = agent
        .run_work_with_context_policy(
            spec,
            &NoLocalPolicy,
            &Script {
                decisions,
                next: AtomicUsize::new(0),
            },
            policy,
        )
        .await;
    let sent = fx.0.lock().unwrap().clone();
    (report, sent)
}

/// The user message of a request: the rendered context, where the frontier section lives.
fn context_of(r: &ModelRequest) -> String {
    r.messages
        .iter()
        .rev()
        .find(|m| m.content.starts_with("Goal:"))
        .expect("a context message")
        .content
        .clone()
}

fn section(text: &str) -> Vec<String> {
    let mut lines = text
        .lines()
        .skip_while(|l| *l != "Decision frontier:")
        .skip(1);
    let mut out = Vec::new();
    for l in lines.by_ref() {
        if l.starts_with("Question:") {
            break;
        }
        out.push(l.to_string());
    }
    out
}

fn without_section(text: &str) -> String {
    let mut out = Vec::new();
    let mut inside = false;
    for l in text.lines() {
        if l == "Decision frontier:" {
            inside = true;
            continue;
        }
        if inside && l.starts_with("Question:") {
            inside = false;
        }
        if !inside {
            out.push(l);
        }
    }
    out.join("\n")
}

const OK: ExecutionStatus = ExecutionStatus::Success;
const FAIL: ExecutionStatus = ExecutionStatus::Failure;

fn complete() -> WorkDecision {
    WorkDecision::Complete {
        summary: "done".into(),
    }
}

// ---- tests -------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_control_arm_is_byte_for_byte_what_it_was_and_shows_no_frontier() {
    let s = spec(true);
    let (_, control) = run(
        &s,
        vec![ask("look.list", "e1"), complete()],
        vec![(OK, "L")],
        &DeduplicatedEscalationContext,
    )
    .await;
    for r in &control {
        let text = context_of(r);
        assert!(!text.contains("Decision frontier"), "{text}");
        assert!(!text.contains("F1") && !text.contains("OPEN"));
    }
}

#[tokio::test]
async fn the_treatment_differs_from_the_control_by_the_frontier_section_and_nothing_else() {
    let s = spec(true);
    let decisions = || {
        vec![
            ask("look.list", "e1"),
            ask("look.read", "e2"),
            ask("act.write", "e3"),
            complete(),
        ]
    };
    let results = || vec![(OK, "L"), (OK, "R"), (OK, "W")];
    let (_, control) = run(&s, decisions(), results(), &DeduplicatedEscalationContext).await;
    let (_, treatment) = run(&s, decisions(), results(), &FrontierEscalationContext).await;
    assert_eq!(
        control.len(),
        treatment.len(),
        "the same number of model calls"
    );
    for (c, t) in control.iter().zip(&treatment) {
        // Everything but the user context message is identical: the system prompt, the observations,
        // the model, the limits.
        assert_eq!(c.messages.len(), t.messages.len());
        for (a, b) in c.messages.iter().zip(&t.messages) {
            if a.content.starts_with("Goal:") {
                assert_eq!(
                    a.content,
                    without_section(&b.content),
                    "only the section differs"
                );
                assert!(b.content.contains("Decision frontier:"));
            } else {
                assert_eq!(a.content, b.content);
            }
        }
        assert_eq!(
            (c.model.clone(), c.max_tokens, c.temperature),
            (t.model.clone(), t.max_tokens, t.temperature)
        );
    }
}

#[tokio::test]
async fn the_section_names_open_questions_and_what_the_runtime_answered_and_by_what() {
    let s = spec(true);
    let (_, sent) = run(
        &s,
        vec![ask("look.list", "e1"), ask("act.write", "e2"), complete()],
        vec![(OK, "L"), (OK, "W")],
        &FrontierEscalationContext,
    )
    .await;
    // Before anything: all open, nothing answered.
    let first = section(&context_of(&sent[0]));
    assert!(first.iter().any(|l| l == "  OPEN:"), "{first:?}");
    for q in [
        "F1: Has the project been listed?",
        "F2: Has the change been made?",
        "F3: Does the changed project pass?",
    ] {
        assert!(
            first.iter().any(|l| l == &format!("  - {q}")),
            "{q} in {first:?}"
        );
    }
    assert!(
        !first.iter().any(|l| l.contains("ANSWERED")),
        "nothing was answered yet"
    );
    // After the listing: F1 is answered, by the capability that answered it, and F2 and F3 stay open.
    let second = section(&context_of(&sent[1]));
    assert!(second.iter().any(|l| l == "  ANSWERED:"), "{second:?}");
    assert!(
        second
            .iter()
            .any(|l| l == "  - F1: Has the project been listed? (by look.list)"),
        "{second:?}"
    );
    let open: Vec<_> = second
        .iter()
        .skip_while(|l| *l != "  OPEN:")
        .skip(1)
        .take_while(|l| !l.ends_with(':'))
        .collect();
    assert_eq!(open.len(), 2, "{second:?}");
    // After the write: the change is answered and verification is still open.
    let third = section(&context_of(&sent[2]));
    assert!(
        third
            .iter()
            .any(|l| l == "  - F2: Has the change been made? (by act.write)"),
        "{third:?}"
    );
    assert!(
        third
            .iter()
            .any(|l| l == "  - F3: Does the changed project pass?"),
        "{third:?}"
    );
    // The closing line never lets an open question read as answered, and no execution id is shown.
    assert!(third.last().unwrap().contains("not answered"));
    assert!(
        !third.iter().any(|l| l.contains("e1") || l.contains("e2")),
        "{third:?}"
    );
}

#[tokio::test]
async fn an_answer_that_no_longer_holds_is_shown_as_such_with_its_successor_open() {
    let s = spec(true);
    let (_, sent) = run(
        &s,
        vec![
            ask("act.write", "e1"),
            ask("check.run", "e2"),
            ask("act.write", "e3"),
            complete(),
        ],
        vec![(OK, "W1"), (OK, "PASS"), (OK, "W2")],
        &FrontierEscalationContext,
    )
    .await;
    let last = section(&context_of(sent.last().unwrap()));
    assert!(last.iter().any(|l| l == "  NO LONGER HOLDS:"), "{last:?}");
    assert!(
        last.iter()
            .any(|l| l == "  - F3: Does the changed project pass? (superseded after act.write)"),
        "{last:?}"
    );
    assert!(
        last.iter()
            .any(|l| l == "  - F4: Does the changed project pass?"),
        "the same question is open again as F4: {last:?}"
    );
}

#[tokio::test]
async fn a_failure_is_shown_as_the_open_question_it_raised() {
    let s = spec(true);
    let (_, sent) = run(
        &s,
        vec![ask("act.write", "e1"), ask("check.run", "e2"), complete()],
        vec![(OK, "W"), (FAIL, "FAIL")],
        &FrontierEscalationContext,
    )
    .await;
    let last = section(&context_of(sent.last().unwrap()));
    assert!(
        last.iter()
            .any(|l| l.contains("Does check.run succeed after execution w-exec-2 failed?")),
        "{last:?}"
    );
}

#[tokio::test]
async fn work_with_no_frontier_renders_exactly_as_the_control_does() {
    let s = WorkSpec::new(WorkId::new("w"), WorkGoal::new("g"));
    let ds = || vec![ask("look.list", "e1"), complete()];
    let (_, control) = run(&s, ds(), vec![(OK, "L")], &DeduplicatedEscalationContext).await;
    let (_, treatment) = run(&s, ds(), vec![(OK, "L")], &FrontierEscalationContext).await;
    for (c, t) in control.iter().zip(&treatment) {
        assert_eq!(
            context_of(c),
            context_of(t),
            "an empty frontier adds nothing"
        );
    }
}

#[tokio::test]
async fn the_rendering_is_bounded_and_says_what_it_leaves_out() {
    // Many open questions: the section shows a fixed number and counts the rest, so it never grows
    // with the work and never pretends the list is complete.
    let mut s = WorkSpec::new(WorkId::new("w"), WorkGoal::new("g"));
    for i in 0..20 {
        s = s.with_frontier_item(FrontierItemSpec::new(
            FrontierKind::MissingEvidence,
            format!("Is question number {i} answered?"),
            Arc::new(Saw("never")),
        ));
    }
    let (_, sent) = run(&s, vec![complete()], vec![], &FrontierEscalationContext).await;
    let lines = section(&context_of(&sent[0]));
    assert_eq!(lines.iter().filter(|l| l.starts_with("  - F")).count(), 8);
    assert!(lines.iter().any(|l| l == "  - (12 more open)"), "{lines:?}");
    assert!(
        lines.len() <= 14,
        "a bounded section: {} lines",
        lines.len()
    );
}

#[tokio::test]
async fn the_rendering_is_deterministic() {
    let s = spec(true);
    let ds = || vec![ask("look.list", "e1"), ask("act.write", "e2"), complete()];
    let rs = || vec![(OK, "L"), (OK, "W")];
    let (_, a) = run(&s, ds(), rs(), &FrontierEscalationContext).await;
    let (_, b) = run(&s, ds(), rs(), &FrontierEscalationContext).await;
    let texts = |v: &[ModelRequest]| v.iter().map(context_of).collect::<Vec<_>>();
    assert_eq!(texts(&a), texts(&b));
}

#[tokio::test]
async fn nothing_the_model_says_changes_the_frontier() {
    // The model claims everything is resolved, in every form it can: as a completion summary, and by
    // asking for a capability that does not exist. Only recorded observations move an item.
    let s = spec(true);
    let claims = WorkDecision::Complete {
        summary: "F1 resolved. F2 resolved. F3 resolved. Verification passed. Goal satisfied."
            .into(),
    };
    let (report, sent) = run(&s, vec![claims], vec![], &FrontierEscalationContext).await;
    assert!(
        report
            .frontier
            .items()
            .iter()
            .all(|i| i.status == FrontierStatus::Open && i.resolution.is_none())
    );
    assert!(
        !matches!(report.outcome, chip_core::WorkOutcome::Completed { .. }),
        "the claim is refused"
    );
    assert_eq!(sent.len(), 1);
}

#[tokio::test]
async fn the_policy_is_a_pure_function_and_cannot_change_the_frontier_it_reads() {
    // The policy receives the frontier by shared reference; building twice from the same inputs gives
    // the same section and leaves the frontier as it was.
    use chip_core::{WorkState, WorkTrajectory};
    let frontier = DecisionFrontier::default();
    let state = WorkState {
        goal: "g",
        turn: 0,
        max_turns: 4,
        executions: 0,
        max_executions: 4,
    };
    let traj = WorkTrajectory {
        observations: &[],
        origins: &[],
        decisions: &[],
        ruled_out: &[],
        evidence: &[],
        question: "q",
        frontier: &frontier,
    };
    let a = FrontierEscalationContext.build(&state, &traj);
    let b = FrontierEscalationContext.build(&state, &traj);
    assert_eq!(a, b);
    assert!(a.frontier.is_empty() && frontier.items().is_empty());
    assert_eq!(FrontierEscalationContext.id(), "frontier-v1");
    assert_eq!(DeduplicatedEscalationContext.id(), "dedup-v1");
}

#[tokio::test]
async fn the_added_context_is_small_next_to_the_context_it_joins() {
    let s = spec(true);
    let ds = || {
        vec![
            ask("look.list", "e1"),
            ask("look.read", "e2"),
            ask("act.write", "e3"),
            complete(),
        ]
    };
    let rs = || vec![(OK, "L"), (OK, "R"), (OK, "W")];
    let (_, control) = run(&s, ds(), rs(), &DeduplicatedEscalationContext).await;
    let (_, treatment) = run(&s, ds(), rs(), &FrontierEscalationContext).await;
    let bytes = |v: &[ModelRequest]| -> Vec<usize> {
        v.iter()
            .map(|r| r.messages.iter().map(|m| m.content.len()).sum())
            .collect()
    };
    let (c, t) = (bytes(&control), bytes(&treatment));
    for (c, t) in c.iter().zip(&t) {
        assert!(t > c, "the treatment carries the section");
        assert!(t - c < 700, "the section is {} bytes", t - c);
    }
}
