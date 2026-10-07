//! PR47: the capability contract the model is shown is the contract the runtime enforces.
//!
//! Two halves of one claim. First, what a model is told about each capability is generated from
//! that capability's declaration and says exactly whether it takes inputs. Second, the runtime
//! still rejects every invocation that departs from the declaration, and a rejection leaves no
//! trace: no execution, no observation, no evidence, no completion. Nothing is repaired: an empty
//! `inputs` is not turned into an absent one.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, Capability, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityInput, CapabilityProvider, CapabilitySet, ExecutionError, ExecutionObserver,
    ExecutionRequest, ExecutionResult, Executor, LocalWorkPolicy, ModelDecisionBoundary,
    TestLocalReasoner, WorkDecision, WorkDecisionBoundary, WorkEvent, WorkGoal, WorkId, WorkLimits,
    WorkOutcome, WorkReport, WorkSpec, WorkView, audit_safety,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

fn descriptor(id: &str, inputs: &[(&str, bool)]) -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::new(CapabilityId::new(id).unwrap(), id, format!("does {id}"));
    d.inputs = inputs
        .iter()
        .map(|(name, required)| CapabilityInput {
            name: name.to_string(),
            description: String::new(),
            required: *required,
        })
        .collect();
    d
}

/// One backend declaring a capability with no inputs, one with required inputs, one with only
/// optional inputs, and one that is declared but unavailable.
struct Backend {
    calls: Mutex<usize>,
}

#[async_trait::async_trait]
impl CapabilityProvider for Backend {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(declared())
    }
    async fn availability(&self, id: &CapabilityId) -> CapabilityAvailability {
        if id.as_str() == "t.offline" {
            CapabilityAvailability::Unavailable("offline".into())
        } else {
            CapabilityAvailability::Available
        }
    }
}

#[async_trait::async_trait]
impl Executor for Backend {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        *self.calls.lock().unwrap() += 1;
        Ok(ExecutionResult::success(request.id, "ran"))
    }
}

fn declared() -> Vec<CapabilityDescriptor> {
    vec![
        descriptor("t.noinput", &[]),
        descriptor("t.read", &[("path", true)]),
        descriptor("t.write", &[("path", true), ("content", true)]),
        descriptor("t.list", &[("path", false)]),
        descriptor("t.offline", &[("secret", true)]),
    ]
}

fn offered() -> Vec<Capability> {
    declared()
        .into_iter()
        .map(|d| {
            let availability = if d.id.as_str() == "t.offline" {
                CapabilityAvailability::Unavailable("offline".into())
            } else {
                CapabilityAvailability::Available
            };
            Capability {
                descriptor: d,
                availability,
            }
        })
        .collect()
}

// ---- what the model is shown -----------------------------------------------------------------------

#[test]
fn a_capability_with_no_inputs_is_shown_as_taking_none_and_the_form_omits_inputs() {
    let q = ModelDecisionBoundary.question(&offered());
    assert!(
        q.contains(r#"t.noinput takes no inputs: {"decision":"request_capability","capability":"t.noinput"} (no "inputs" field)"#),
        "{q}"
    );
    // The invalid form is shown as invalid, with the reason.
    assert!(
        q.contains(
            r#"{"decision":"request_capability","capability":"t.noinput","inputs":{}} is invalid"#
        ),
        "{q}"
    );
    assert!(
        q.contains(r#"must not carry an "inputs" field at all, not even an empty one"#),
        "{q}"
    );
    // The old wording that made an empty `inputs` look acceptable is gone.
    assert!(!q.contains("(inputs optional)"), "{q}");
}

#[test]
fn a_capability_with_inputs_is_shown_with_exactly_its_declared_fields() {
    let q = ModelDecisionBoundary.question(&offered());
    assert!(
        q.contains(r#"t.read takes inputs (path required): {"decision":"request_capability","capability":"t.read","inputs":{"path":<string|integer|boolean>}}"#),
        "{q}"
    );
    assert!(
        q.contains(r#"t.write takes inputs (path required, content required): {"decision":"request_capability","capability":"t.write","inputs":{"path":<string|integer|boolean>,"content":<string|integer|boolean>}}"#),
        "{q}"
    );
    // Only optional inputs: they may be given, and with none to give the field is simply absent.
    assert!(
        q.contains(r#"t.list takes inputs (path optional): "#),
        "{q}"
    );
    assert!(
        q.contains(r#"with no inputs to give, omit the "inputs" field"#),
        "{q}"
    );
    // The existing list, in its existing form, still follows.
    assert!(
        q.contains("t.write (inputs: path, content) - does t.write"),
        "{q}"
    );
}

#[test]
fn what_the_model_is_shown_is_deterministic_and_follows_the_declaration() {
    let a = ModelDecisionBoundary.question(&offered());
    let b = ModelDecisionBoundary.question(&offered());
    assert_eq!(a, b, "the same declarations must read the same");
    // Declared order is presentation order, in the forms and in the list.
    let forms = a
        .split("Available capabilities: ")
        .next()
        .unwrap()
        .split("How to request each capability, exactly: ")
        .nth(1)
        .expect("the forms section");
    let positions: Vec<usize> = ["t.noinput", "t.read", "t.write", "t.list"]
        .iter()
        .map(|id| forms.find(&format!("{id} takes")).unwrap())
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{positions:?}");
    // An unavailable capability is not described anywhere.
    assert!(!a.contains("t.offline") && !a.contains("secret"), "{a}");
    // For every offered capability, what it is said to take is what it declares.
    for c in offered()
        .iter()
        .filter(|c| c.availability == CapabilityAvailability::Available)
    {
        let id = c.descriptor.id.as_str();
        let line = forms
            .split("; ")
            .find(|e| e.starts_with(&format!("{id} takes")))
            .unwrap_or_else(|| panic!("{id} has no form in {forms}"));
        let says_none = line.starts_with(&format!("{id} takes no inputs"));
        assert_eq!(says_none, c.descriptor.inputs.is_empty(), "{id}: {line}");
        assert_eq!(
            line.contains(r#""inputs":{"#),
            !c.descriptor.inputs.is_empty(),
            "{id}: {line}"
        );
        for input in &c.descriptor.inputs {
            let marker = format!(
                "{} {}",
                input.name,
                if input.required {
                    "required"
                } else {
                    "optional"
                }
            );
            assert!(line.contains(&marker), "{id}: {line}");
        }
    }
}

#[test]
fn with_no_capability_that_takes_no_inputs_nothing_about_empty_inputs_is_said() {
    let only_inputs: Vec<Capability> = offered()
        .into_iter()
        .filter(|c| c.descriptor.id.as_str() == "t.read")
        .collect();
    let q = ModelDecisionBoundary.question(&only_inputs);
    assert!(!q.contains("must not carry"), "{q}");
    assert!(q.contains("t.read takes inputs"), "{q}");
    assert_eq!(
        ModelDecisionBoundary
            .question(&[])
            .contains("How to request"),
        false
    );
}

// ---- what the runtime still refuses, and the absence of any effect -------------------------------------

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

struct AskModel;
impl LocalWorkPolicy for AskModel {
    fn propose(&self, _v: &WorkView<'_>) -> Option<WorkDecision> {
        None
    }
}

async fn run(replies: &[&str]) -> (WorkReport, WorkSpec, Arc<Backend>) {
    let backend = Arc::new(Backend {
        calls: Mutex::new(0),
    });
    let set = Arc::new(CapabilitySet::new().with(backend.clone()));
    let agent = Agent::new(Arc::new(Script(Mutex::new(
        replies.iter().map(|s| s.to_string()).collect(),
    ))))
    .with_capabilities(set.clone())
    .with_executor(set)
    .with_observer(Arc::new(ExecutionObserver))
    .with_local_reasoner(Arc::new(TestLocalReasoner::default()));
    let spec = WorkSpec::new(WorkId::new("w"), WorkGoal::new("g"))
        .with_limits(WorkLimits {
            max_turns: 4,
            max_executions: 4,
        })
        .with_required_output("never");
    let report = agent
        .run_work(&spec, &AskModel, &ModelDecisionBoundary)
        .await;
    (report, spec, backend)
}

fn effects(r: &WorkReport) -> (usize, usize, usize) {
    let executions = r
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                WorkEvent::Execution(chip_core::ExecutionEvent::ExecutionStarted { .. })
            )
        })
        .count();
    let evidence = r
        .events
        .iter()
        .filter(|e| matches!(e, WorkEvent::EvidenceRecorded { .. }))
        .count();
    (executions, r.observations.len(), evidence)
}

#[tokio::test]
async fn an_invocation_that_departs_from_the_declaration_has_no_effect_at_all() {
    let cases: Vec<(&str, &str)> = vec![
        (
            "no-input capability with empty inputs",
            r#"{"decision":"request_capability","capability":"t.noinput","inputs":{}}"#,
        ),
        (
            "no-input capability with an unknown input",
            r#"{"decision":"request_capability","capability":"t.noinput","inputs":{"anything":"anything"}}"#,
        ),
        (
            "input-bearing capability with no inputs",
            r#"{"decision":"request_capability","capability":"t.read"}"#,
        ),
        (
            "input-bearing capability with empty inputs",
            r#"{"decision":"request_capability","capability":"t.read","inputs":{}}"#,
        ),
        (
            "input-bearing capability with an undeclared input",
            r#"{"decision":"request_capability","capability":"t.read","inputs":{"path":"a","mode":"w"}}"#,
        ),
        (
            "a required input missing among several",
            r#"{"decision":"request_capability","capability":"t.write","inputs":{"path":"a"}}"#,
        ),
        (
            "optional-only capability with an undeclared input",
            r#"{"decision":"request_capability","capability":"t.list","inputs":{"recursive":true}}"#,
        ),
        (
            "inputs that is a string",
            r#"{"decision":"request_capability","capability":"t.read","inputs":"path"}"#,
        ),
        (
            "inputs that is an array",
            r#"{"decision":"request_capability","capability":"t.read","inputs":["a"]}"#,
        ),
        (
            "inputs that is null",
            r#"{"decision":"request_capability","capability":"t.read","inputs":null}"#,
        ),
        (
            "an input that is an object",
            r#"{"decision":"request_capability","capability":"t.read","inputs":{"path":{"a":1}}}"#,
        ),
        (
            "an unavailable capability",
            r#"{"decision":"request_capability","capability":"t.offline","inputs":{"secret":"x"}}"#,
        ),
        (
            "an undeclared capability",
            r#"{"decision":"request_capability","capability":"t.nothing"}"#,
        ),
    ];
    for (what, reply) in cases {
        let (report, spec, backend) = run(&[reply]).await;
        assert_eq!(effects(&report), (0, 0, 0), "{what}: {:?}", report.outcome);
        assert_eq!(
            *backend.calls.lock().unwrap(),
            0,
            "{what}: the executor was reached"
        );
        assert!(
            !matches!(report.outcome, WorkOutcome::Completed { .. }),
            "{what}"
        );
        assert!(
            !report
                .events
                .iter()
                .any(|e| matches!(e, WorkEvent::WorkCompleted { .. })),
            "{what}"
        );
        let ids: Vec<CapabilityId> = declared().iter().map(|d| d.id.clone()).collect();
        audit_safety(&report, &spec, &ids).assert_clean();
    }
}

#[tokio::test]
async fn an_empty_inputs_is_not_repaired_into_an_absent_one() {
    // The same decision without the field runs; with `{}` it is refused and nothing is executed. Chip
    // never rewrites the second into the first.
    let (valid, _, backend) = run(&[
        r#"{"decision":"request_capability","capability":"t.noinput"}"#,
        r#"{"decision":"block","reason":"x"}"#,
    ])
    .await;
    assert_eq!(effects(&valid), (1, 1, 1));
    assert_eq!(*backend.calls.lock().unwrap(), 1);
    let (refused, _, backend) = run(&[
        r#"{"decision":"request_capability","capability":"t.noinput","inputs":{}}"#,
        r#"{"decision":"request_capability","capability":"t.noinput"}"#,
    ])
    .await;
    assert_eq!(
        effects(&refused),
        (0, 0, 0),
        "a second, valid request was never reached: no retry, no repair"
    );
    assert_eq!(*backend.calls.lock().unwrap(), 0);
    assert!(
        matches!(&refused.outcome, WorkOutcome::Blocked { reason } if reason.contains("must not carry an `inputs` member")),
        "{:?}",
        refused.outcome
    );
}

#[tokio::test]
async fn valid_invocations_in_every_declared_shape_run() {
    for reply in [
        r#"{"decision":"request_capability","capability":"t.noinput"}"#,
        r#"{"decision":"request_capability","capability":"t.read","inputs":{"path":"a"}}"#,
        r#"{"decision":"request_capability","capability":"t.write","inputs":{"path":"a","content":"c"}}"#,
        r#"{"decision":"request_capability","capability":"t.list"}"#,
        r#"{"decision":"request_capability","capability":"t.list","inputs":{"path":"src"}}"#,
        r#"{"schema":"chip.work-decision.v1","decision":"request_capability","capability":"t.noinput"}"#,
    ] {
        let (report, spec, backend) = run(&[reply, r#"{"decision":"block","reason":"x"}"#]).await;
        assert_eq!(effects(&report), (1, 1, 1), "{reply}: {:?}", report.outcome);
        assert_eq!(*backend.calls.lock().unwrap(), 1, "{reply}");
        let ids: Vec<CapabilityId> = declared().iter().map(|d| d.id.clone()).collect();
        audit_safety(&report, &spec, &ids).assert_clean();
    }
}
