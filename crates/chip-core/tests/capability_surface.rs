//! PR45's generic runtime mechanisms: typed inputs reaching executors, capability-owned input
//! rules and sizes, capabilities that are never answered from memory, a set of capability backends,
//! order-sensitive goal predicates, and observation invariants in the safety audit.
//!
//! Offline and deterministic: a scripted model goes through the real `ModelDecisionBoundary`; a
//! recording backend stands in for an executor. Nothing here knows any project, file or tool.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityInput, CapabilityProvider, CapabilitySet, ExecutionError, ExecutionId,
    ExecutionObserver, ExecutionRequest, ExecutionResult, Executor, InputValue, LocalWorkPolicy,
    ModelDecisionBoundary, Observation, ObservationInvariant, ObservationKind,
    ObservationPredicate, TestLocalReasoner, WorkDecision, WorkEvent, WorkGoal, WorkId, WorkLimits,
    WorkOutcome, WorkReport, WorkSpec, WorkView, audit_safety, measure_utility,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

struct Script(Mutex<VecDeque<String>>);

#[async_trait::async_trait]
impl ModelProvider for Script {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        let reply = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| FxError::Provider("unscripted".into()))?;
        Ok(ModelResponse::new("m", reply, Usage::new(1, 1)))
    }
}

/// Always asks the model.
struct AskModel;
impl LocalWorkPolicy for AskModel {
    fn propose(&self, _v: &WorkView<'_>) -> Option<WorkDecision> {
        None
    }
}

/// A backend with its own capabilities. It records every request it executes and answers each with
/// `output(call number)`; `failing` makes the answer a failed execution.
struct Backend {
    descriptors: Vec<CapabilityDescriptor>,
    calls: Mutex<Vec<ExecutionRequest>>,
    answer: fn(usize) -> String,
    rule: Option<(&'static str, &'static str)>,
    failing: bool,
}

impl Backend {
    fn new(descriptors: Vec<CapabilityDescriptor>) -> Arc<Self> {
        Arc::new(Self {
            descriptors,
            calls: Mutex::new(Vec::new()),
            answer: |n| format!("answer {n}"),
            rule: None,
            failing: false,
        })
    }
    fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl CapabilityProvider for Backend {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(self.descriptors.clone())
    }
    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
    async fn validate_inputs(
        &self,
        _id: &CapabilityId,
        inputs: &BTreeMap<String, InputValue>,
    ) -> Result<(), CapabilityError> {
        if let Some((name, forbidden)) = self.rule {
            if inputs.get(name) == Some(&InputValue::Text(forbidden.to_string())) {
                return Err(CapabilityError::InvalidInput(format!(
                    "{name} may not be {forbidden}"
                )));
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Executor for Backend {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        let n = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(request.clone());
            calls.len()
        };
        let out = (self.answer)(n);
        Ok(if self.failing {
            ExecutionResult::failure(request.id, out)
        } else {
            ExecutionResult::success(request.id, out)
        })
    }
}

fn capability(id: &str, inputs: &[&str]) -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::new(
        CapabilityId::new(id).unwrap(),
        id,
        format!("the {id} capability"),
    );
    d.inputs = inputs
        .iter()
        .map(|n| CapabilityInput {
            name: n.to_string(),
            description: String::new(),
            required: true,
        })
        .collect();
    d
}

fn request(id: &str) -> String {
    format!(r#"{{"decision":"request_capability","capability":"{id}"}}"#)
}

fn request_with(id: &str, name: &str, value: &str) -> String {
    format!(
        r#"{{"decision":"request_capability","capability":"{id}","inputs":{{"{name}":{}}}}}"#,
        serde_json::to_string(value).unwrap()
    )
}

async fn run(backends: &[Arc<Backend>], replies: Vec<String>, spec: WorkSpec) -> WorkReport {
    let mut set = CapabilitySet::new();
    for b in backends {
        set = set.with(b.clone());
    }
    let set = Arc::new(set);
    let agent = Agent::new(Arc::new(Script(Mutex::new(replies.into()))))
        .with_capabilities(set.clone())
        .with_executor(set)
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()));
    agent
        .run_work(&spec, &AskModel, &ModelDecisionBoundary)
        .await
}

fn spec() -> WorkSpec {
    WorkSpec::new(WorkId::new("w"), WorkGoal::new("g")).with_limits(WorkLimits {
        max_turns: 8,
        max_executions: 6,
    })
}

fn count(r: &WorkReport, pick: fn(&WorkEvent) -> bool) -> usize {
    r.events.iter().filter(|e| pick(e)).count()
}

fn reused(r: &WorkReport) -> usize {
    count(r, |e| matches!(e, WorkEvent::EvidenceReused { .. }))
}

// ---- inputs reach the executor, after the capability's own rules ----------------------------------

#[tokio::test]
async fn validated_inputs_reach_the_executor_and_nothing_else_does() {
    let b = Backend::new(vec![capability("t.op", &["name"])]);
    let r = run(
        &[b.clone()],
        vec![
            request_with("t.op", "name", "x"),
            r#"{"decision":"block","reason":"done"}"#.into(),
        ],
        spec(),
    )
    .await;
    assert!(matches!(r.outcome, WorkOutcome::Blocked { .. }));
    let calls = b.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].intent, "t.op");
    assert_eq!(
        calls[0].inputs,
        BTreeMap::from([("name".to_string(), InputValue::Text("x".into()))])
    );
}

#[tokio::test]
async fn the_capabilitys_own_input_rule_refuses_before_anything_executes() {
    let mut b = Backend::new(vec![capability("t.op", &["name"])]);
    Arc::get_mut(&mut b).unwrap().rule = Some(("name", "forbidden"));
    let r = run(
        &[b.clone()],
        vec![request_with("t.op", "name", "forbidden")],
        spec(),
    )
    .await;
    assert!(
        matches!(&r.outcome, WorkOutcome::Blocked { reason } if reason.contains("may not be forbidden")),
        "{:?}",
        r.outcome
    );
    assert_eq!(b.calls(), 0);
    assert!(r.observations.is_empty());
    assert_eq!(
        count(&r, |e| matches!(e, WorkEvent::EvidenceRecorded { .. })),
        0
    );
    audit_safety(&r, &spec(), &[CapabilityId::new("t.op").unwrap()]).assert_clean();
}

// ---- how large an input may be is the capability's to declare ------------------------------------------------------

#[tokio::test]
async fn input_size_is_declared_by_the_capability_and_bounded_by_it() {
    let big = "a".repeat(3000);
    // A capability that declares nothing gets the small default: the same reply is refused.
    let small = Backend::new(vec![capability("t.small", &["text"])]);
    let r = run(
        &[small.clone()],
        vec![request_with("t.small", "text", &big)],
        spec(),
    )
    .await;
    assert!(
        matches!(&r.outcome, WorkOutcome::Failed { reason } if reason.contains("not a valid decision")),
        "{:?}",
        r.outcome
    );
    assert_eq!(small.calls(), 0);
    // One that declares room for it accepts it, and the executor sees every byte.
    let roomy = Backend::new(vec![
        capability("t.roomy", &["text"]).with_max_input_bytes(5000),
    ]);
    let r = run(
        &[roomy.clone()],
        vec![
            request_with("t.roomy", "text", &big),
            r#"{"decision":"block","reason":"x"}"#.into(),
        ],
        spec(),
    )
    .await;
    assert!(
        matches!(r.outcome, WorkOutcome::Blocked { .. }),
        "{:?}",
        r.outcome
    );
    assert_eq!(
        roomy.calls.lock().unwrap()[0].inputs["text"],
        InputValue::Text(big.clone())
    );
    // Past what it declared, refused again, and nothing ran.
    let over = "a".repeat(5001);
    let roomy = Backend::new(vec![
        capability("t.roomy", &["text"]).with_max_input_bytes(5000),
    ]);
    let r = run(
        &[roomy.clone()],
        vec![request_with("t.roomy", "text", &over)],
        spec(),
    )
    .await;
    assert!(
        matches!(&r.outcome, WorkOutcome::Failed { .. }),
        "{:?}",
        r.outcome
    );
    assert_eq!(roomy.calls(), 0);
    // The declared size does not lend room to the other text of the reply.
    let long_reason = format!(
        r#"{{"decision":"block","reason":{}}}"#,
        serde_json::to_string(&"r".repeat(1500)).unwrap()
    );
    let r = run(
        &[Backend::new(vec![
            capability("t.roomy", &["text"]).with_max_input_bytes(5000),
        ])],
        vec![long_reason],
        spec(),
    )
    .await;
    assert!(
        matches!(&r.outcome, WorkOutcome::Failed { .. }),
        "{:?}",
        r.outcome
    );
}

// ---- evidence of a capability that must not be remembered ------------------------------------------------------------

#[tokio::test]
async fn a_capability_that_opts_out_is_performed_again_not_answered_from_evidence() {
    let stateless = Backend::new(vec![capability("t.pure", &[])]);
    let r = run(
        &[stateless.clone()],
        vec![
            request("t.pure"),
            request("t.pure"),
            r#"{"decision":"block","reason":"x"}"#.into(),
        ],
        spec(),
    )
    .await;
    assert_eq!(
        (stateless.calls(), reused(&r)),
        (1, 1),
        "an ordinary capability is reused"
    );

    let stateful = Backend::new(vec![capability("t.state", &[]).without_evidence_reuse()]);
    let r = run(
        &[stateful.clone()],
        vec![
            request("t.state"),
            request("t.state"),
            r#"{"decision":"block","reason":"x"}"#.into(),
        ],
        spec(),
    )
    .await;
    assert_eq!(
        (stateful.calls(), reused(&r)),
        (2, 0),
        "the second request must reach reality"
    );
    // Each execution was observed and recorded, so the chain is intact for both.
    assert_eq!(r.observations.len(), 2);
    assert_eq!(
        count(&r, |e| matches!(e, WorkEvent::EvidenceRecorded { .. })),
        2
    );
    assert_ne!(r.observations[0].output, r.observations[1].output);
}

// ---- a set of backends ---------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_set_routes_each_capability_to_the_backend_that_declared_it() {
    let a = Backend::new(vec![capability("a.one", &[]), capability("a.two", &[])]);
    let b = Backend::new(vec![capability("b.one", &[])]);
    let set = CapabilitySet::new().with(a.clone()).with(b.clone());
    let ids: Vec<String> = set
        .capabilities()
        .await
        .unwrap()
        .iter()
        .map(|d| d.id.to_string())
        .collect();
    assert_eq!(ids, ["a.one", "a.two", "b.one"]);
    for id in ["a.one", "b.one"] {
        assert_eq!(
            set.availability(&CapabilityId::new(id).unwrap()).await,
            CapabilityAvailability::Available
        );
        set.execute(ExecutionRequest::new(ExecutionId::new("e"), id))
            .await
            .unwrap();
    }
    assert_eq!((a.calls(), b.calls()), (1, 1));
    // Nothing declared it: unavailable and refused, never routed somewhere else.
    assert!(matches!(
        set.availability(&CapabilityId::new("c.one").unwrap()).await,
        CapabilityAvailability::Unavailable(_)
    ));
    assert!(matches!(
        set.execute(ExecutionRequest::new(ExecutionId::new("e"), "c.one"))
            .await,
        Err(ExecutionError::InvalidRequest(_))
    ));
    assert_eq!((a.calls(), b.calls()), (1, 1));
}

#[tokio::test]
async fn two_backends_declaring_one_id_is_a_configuration_error_that_fails_closed() {
    let a = Backend::new(vec![capability("x.same", &[])]);
    let b = Backend::new(vec![capability("x.same", &[])]);
    let set = CapabilitySet::new().with(a.clone()).with(b.clone());
    assert!(matches!(
        set.capabilities().await,
        Err(CapabilityError::Unavailable(_))
    ));
    assert!(matches!(
        set.availability(&CapabilityId::new("x.same").unwrap())
            .await,
        CapabilityAvailability::Misconfigured(_)
    ));
    assert!(matches!(
        set.execute(ExecutionRequest::new(ExecutionId::new("e"), "x.same"))
            .await,
        Err(ExecutionError::ExecutorUnavailable(_))
    ));
    assert_eq!((a.calls(), b.calls()), (0, 0), "no backend was picked");
}

// ---- goals that depend on order -------------------------------------------------------------------------------------------

/// "A was observed, and nothing has superseded it since": A establishes it, B (a change) undoes it.
#[derive(Debug)]
struct VerifiedSinceLastChange;

fn marker(o: &Observation) -> &str {
    o.output.as_deref().unwrap_or("")
}

impl ObservationPredicate for VerifiedSinceLastChange {
    fn describe(&self) -> String {
        "A observed after the last B".into()
    }
    fn satisfied_by(&self, _o: &Observation) -> bool {
        false
    }
    fn satisfied_by_trajectory(&self, observations: &[Observation]) -> bool {
        let last_b = observations
            .iter()
            .rposition(|o| marker(o).starts_with("B"));
        let last_a = observations
            .iter()
            .rposition(|o| marker(o).starts_with("A"));
        match (last_a, last_b) {
            (Some(a), Some(b)) => a > b,
            (Some(_), None) => true,
            _ => false,
        }
    }
}

fn marked(a_or_b: &'static str) -> Arc<Backend> {
    let mut b = Backend::new(vec![
        capability(&format!("t.{}", a_or_b.to_lowercase()), &[]).without_evidence_reuse(),
    ]);
    Arc::get_mut(&mut b).unwrap().answer = match a_or_b {
        "A" => |n| format!("A{n}"),
        _ => |n| format!("B{n}"),
    };
    b
}

fn satisfied_trail(r: &WorkReport) -> Vec<bool> {
    r.events
        .iter()
        .filter_map(|e| match e {
            WorkEvent::GoalEvaluated { satisfied, .. } => Some(*satisfied),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_outcome_that_a_later_observation_supersedes_is_unmet_again() {
    let (a, b) = (marked("A"), marked("B"));
    let goal = || spec().with_required_observation(Arc::new(VerifiedSinceLastChange));
    let claim = r#"{"decision":"complete","summary":"done"}"#.to_string();
    // A, then B: the pass no longer covers the changed state, so a claim is refused.
    let r = run(
        &[a.clone(), b.clone()],
        vec![request("t.a"), request("t.b"), claim.clone()],
        goal(),
    )
    .await;
    assert!(
        matches!(&r.outcome, WorkOutcome::Blocked { reason } if reason.starts_with("completion refused")),
        "{:?}",
        r.outcome
    );
    assert_eq!(
        satisfied_trail(&r),
        [true, false],
        "satisfied, then superseded"
    );
    audit_safety(
        &r,
        &goal(),
        &[
            CapabilityId::new("t.a").unwrap(),
            CapabilityId::new("t.b").unwrap(),
        ],
    )
    .assert_clean();
    // A, B, A: verified again after the change, so completion stands.
    let (a, b) = (marked("A"), marked("B"));
    let r = run(
        &[a, b],
        vec![request("t.a"), request("t.b"), request("t.a"), claim],
        goal(),
    )
    .await;
    assert!(
        matches!(r.outcome, WorkOutcome::Completed { .. }),
        "{:?}",
        r.outcome
    );
    assert_eq!(satisfied_trail(&r), [true, false, true]);
    assert_eq!(measure_utility(&r, &goal()).verified_outputs, 1);
    // The audit judges the trajectory too: the same completion over observations in which the
    // verification came first is a false completion, whatever the loop said.
    let mut tampered = r.clone();
    // [A, B, A] becomes [A, A, B]: the change now comes last.
    let (change, second_a) = (r.observations[1].clone(), r.observations[2].clone());
    tampered.observations[1] = second_a;
    tampered.observations[2] = change;
    let audit = audit_safety(
        &tampered,
        &goal(),
        &[
            CapabilityId::new("t.a").unwrap(),
            CapabilityId::new("t.b").unwrap(),
        ],
    );
    assert!(audit.false_completions >= 1, "{audit:?}");
}

// ---- invariants over recorded observations -----------------------------------------------------------------------------------

#[derive(Debug)]
struct NoForbiddenWord;

impl ObservationInvariant for NoForbiddenWord {
    fn name(&self) -> &'static str {
        "forbidden_word"
    }
    fn violations(&self, o: &Observation) -> usize {
        marker(o).matches("FORBIDDEN").count()
    }
}

#[tokio::test]
async fn the_audit_holds_every_recorded_observation_to_the_specs_invariants() {
    let declared = vec![CapabilityId::new("t.op").unwrap()];
    let honest = Backend::new(vec![capability("t.op", &[])]);
    let s = || spec().with_observation_invariant(Arc::new(NoForbiddenWord));
    let r = run(
        &[honest],
        vec![
            request("t.op"),
            r#"{"decision":"block","reason":"x"}"#.into(),
        ],
        s(),
    )
    .await;
    let audit = audit_safety(&r, &s(), &declared);
    assert!(audit.is_clean());
    assert_eq!(
        audit.invariant_violations.get("forbidden_word"),
        Some(&0),
        "a held invariant is still listed"
    );
    // An observation that violates it makes the audit unclean, with the count and the name.
    let mut faulty = Backend::new(vec![capability("t.op", &[])]);
    Arc::get_mut(&mut faulty).unwrap().answer = |_| "FORBIDDEN and FORBIDDEN".into();
    let r = run(
        &[faulty],
        vec![
            request("t.op"),
            r#"{"decision":"block","reason":"x"}"#.into(),
        ],
        s(),
    )
    .await;
    let audit = audit_safety(&r, &s(), &declared);
    assert!(!audit.is_clean());
    assert_eq!(audit.violations_of("forbidden_word"), 2);
    assert!(audit.details.iter().any(|d| d.contains("forbidden_word")));
    // Without the invariant in the spec, the same run audits clean: it is the spec's to declare.
    assert!(audit_safety(&r, &spec(), &declared).is_clean());
}

// ---- utility by capability, failures and recovery ------------------------------------------------------------------------------

#[tokio::test]
async fn utility_counts_executions_by_capability_failures_and_recoveries() {
    let mut bad = Backend::new(vec![capability("t.bad", &[]).without_evidence_reuse()]);
    Arc::get_mut(&mut bad).unwrap().failing = true;
    let good = Backend::new(vec![capability("t.good", &[]).without_evidence_reuse()]);
    let replies = vec![
        request("t.bad"),
        request("t.good"),
        request("t.good"),
        r#"{"decision":"block","reason":"x"}"#.into(),
    ];
    let r = run(&[bad, good], replies, spec()).await;
    let u = measure_utility(&r, &spec());
    assert_eq!(u.executions, 3);
    assert_eq!(
        u.executions_by_capability,
        BTreeMap::from([("t.bad".to_string(), 1), ("t.good".to_string(), 2)])
    );
    assert_eq!(u.failed_observations, 1);
    assert_eq!(
        u.recoveries, 1,
        "one failure, followed by further executions"
    );
    // A failure with nothing after it is a failure, not a recovery.
    let mut bad = Backend::new(vec![capability("t.bad", &[])]);
    Arc::get_mut(&mut bad).unwrap().failing = true;
    let r = run(
        &[bad],
        vec![
            request("t.bad"),
            r#"{"decision":"block","reason":"x"}"#.into(),
        ],
        spec(),
    )
    .await;
    let u = measure_utility(&r, &spec());
    assert_eq!((u.failed_observations, u.recoveries), (1, 0));
    assert!(matches!(
        r.observations[0].kind,
        ObservationKind::ExecutionFailed
    ));
}

// ---- PR46: a note about one invocation is not a ban on the capability ---------------------------------------------------

/// Records every context the model is shown.
struct Seen {
    replies: Mutex<VecDeque<String>>,
    sent: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ModelProvider for Seen {
    async fn complete(&self, r: ModelRequest) -> Result<ModelResponse, FxError> {
        self.sent.lock().unwrap().push(
            r.messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| FxError::Provider("unscripted".into()))?;
        Ok(ModelResponse::new("m", reply, Usage::new(1, 1)))
    }
}

#[tokio::test]
async fn a_note_that_an_invocation_fell_short_names_that_invocation_and_does_not_ban_the_capability()
 {
    let b = Backend::new(vec![
        capability("t.op", &["name"]).without_evidence_reuse(),
        capability("t.plain", &[]),
    ]);
    let model = Arc::new(Seen {
        replies: Mutex::new(
            [
                request_with("t.op", "name", "first"),
                request_with("t.op", "name", "second"), // the same capability again, a different input
                request("t.plain"),
                r#"{"decision":"block","reason":"x"}"#.to_string(),
            ]
            .into(),
        ),
        sent: Mutex::new(Vec::new()),
    });
    let set = Arc::new(CapabilitySet::new().with(b.clone()));
    let agent = Agent::new(model.clone())
        .with_capabilities(set.clone())
        .with_executor(set)
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()));
    let goal = spec().with_required_output("never produced");
    let r = agent
        .run_work(&goal, &AskModel, &ModelDecisionBoundary)
        .await;
    assert_eq!(
        b.calls(),
        3,
        "the revisited capability, with another input, was performed"
    );
    assert!(matches!(r.outcome, WorkOutcome::Blocked { .. }));
    let last = model.sent.lock().unwrap().last().unwrap().clone();
    // The notes are about exactly what was invoked...
    assert!(last.contains(r#"capability t.op (name="first"): executed, but its observation did not satisfy the goal"#), "{last}");
    assert!(last.contains(r#"capability t.op (name="second"): executed, but its observation did not satisfy the goal"#), "{last}");
    // ...a capability with no inputs is still named by itself, as it always was...
    assert!(
        last.contains("capability t.plain: executed, but its observation did not satisfy the goal"),
        "{last}"
    );
    // ...and nothing says the capability as a whole is ruled out.
    assert!(!last.contains("capability t.op: "), "{last}");
}

#[tokio::test]
async fn the_audit_catches_evidence_reused_where_the_spec_prohibits_it() {
    let pure = CapabilityId::new("t.pure").unwrap();
    let b = Backend::new(vec![capability("t.pure", &[])]); // reuse allowed by this backend
    let goal = || spec().with_evidence_reuse_prohibited(pure.clone());
    let r = run(
        &[b.clone()],
        vec![
            request("t.pure"),
            request("t.pure"),
            r#"{"decision":"block","reason":"x"}"#.into(),
        ],
        goal(),
    )
    .await;
    assert_eq!((b.calls(), reused(&r)), (1, 1));
    let audit = audit_safety(&r, &goal(), &[pure.clone()]);
    assert_eq!(audit.stale_evidence_reuse, 1);
    assert!(!audit.is_clean());
    // The same trajectory under a spec that does not prohibit it is clean: the spec declares the rule.
    assert!(audit_safety(&r, &spec(), &[pure]).is_clean());
}

#[tokio::test]
async fn the_audit_catches_events_after_the_work_ended() {
    let b = Backend::new(vec![capability("t.op", &[])]);
    let declared = [CapabilityId::new("t.op").unwrap()];
    let r = run(
        &[b],
        vec![
            request("t.op"),
            r#"{"decision":"block","reason":"x"}"#.into(),
        ],
        spec(),
    )
    .await;
    let audit = audit_safety(&r, &spec(), &declared);
    assert_eq!(audit.events_after_terminal, 0);
    assert!(audit.is_clean());
    let mut tampered = r.clone();
    tampered.events.push(WorkEvent::DecisionStarted {
        work_id: WorkId::new("w"),
        turn: 99,
    });
    let audit = audit_safety(&tampered, &spec(), &declared);
    assert_eq!(audit.events_after_terminal, 1);
    assert!(!audit.is_clean());
}

#[tokio::test]
async fn a_model_reply_budget_is_a_budget_and_changes_no_authority() {
    // The reply budget reaches the model request, and is the only thing it changes.
    struct Budget(Mutex<Vec<Option<u32>>>);
    #[async_trait::async_trait]
    impl ModelProvider for Budget {
        async fn complete(&self, r: ModelRequest) -> Result<ModelResponse, FxError> {
            self.0.lock().unwrap().push(r.max_tokens);
            Ok(ModelResponse::new(
                "m",
                r#"{"decision":"block","reason":"x"}"#,
                Usage::new(1, 1),
            ))
        }
    }
    let seen = Arc::new(Budget(Mutex::new(Vec::new())));
    let r = Agent::new(seen.clone())
        .with_max_output_tokens(2048)
        .run_work(&spec(), &AskModel, &ModelDecisionBoundary)
        .await;
    assert!(matches!(r.outcome, WorkOutcome::Blocked { .. }));
    let default = Arc::new(Budget(Mutex::new(Vec::new())));
    Agent::new(default.clone())
        .run_work(&spec(), &AskModel, &ModelDecisionBoundary)
        .await;
    assert_eq!(*seen.0.lock().unwrap(), [Some(2048)]);
    assert_eq!(
        *default.0.lock().unwrap(),
        [Some(256)],
        "without the budget the boundary's default is unchanged"
    );
}
