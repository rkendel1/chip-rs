//! PR36: the model may select a capability; it may not redefine how that capability is invoked.
//!
//! A capability that declares no inputs has exactly one valid invocation: its id, and no `inputs`
//! member. The runtime, not the model, owns the invocation shape. Two different things can go
//! wrong and the event stream tells them apart without any extra event:
//!
//! * selection rejected: the reply is not a usable decision (undeclared capability, malformed
//!   reply, forbidden field). It ends the work as `WorkFailed`; no capability was ever selected,
//!   so there is no `CapabilityRequested`.
//! * invocation rejected: a declared capability was selected (`DecisionMade`,
//!   `CapabilityRequested`) but the request carried inputs it does not accept. It ends as
//!   `WorkBlocked` with an "invalid capability input" reason, before any execution is requested.
//!
//! Deterministic and offline: a scripted model, the real `ModelDecisionBoundary`, a recording
//! executor.

use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityInput, CapabilityProvider, CapabilityRequest, EvidenceLookup, ExecutionError,
    ExecutionId, ExecutionObserver, ExecutionRequest, ExecutionResult, Executor, LocalWorkPolicy,
    ModelDecisionBoundary, ObservationKind, TestLocalReasoner, WorkDecision, WorkEvent, WorkGoal,
    WorkId, WorkOutcome, WorkReport, WorkSpec, WorkView,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

struct Model(String, Mutex<usize>);

#[async_trait::async_trait]
impl ModelProvider for Model {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        *self.1.lock().unwrap() += 1;
        Ok(ModelResponse::new(
            "msg_01Abc",
            self.0.clone(),
            Usage::new(5, 5),
        ))
    }
}

struct Exec(Mutex<Vec<String>>);

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.0.lock().unwrap().push(r.intent.clone());
        Ok(ExecutionResult::success(r.id, "done").with_receipt_id("sha256:from-the-executor"))
    }
}

/// `compute.op_a` takes no inputs; `compute.op_n` declares one optional input, `n`.
struct Caps;

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut with_input =
            CapabilityDescriptor::new(CapabilityId::new("compute.op_n")?, "n", "Takes a count.");
        with_input.inputs.push(CapabilityInput {
            name: "n".into(),
            description: "how many".into(),
            required: false,
        });
        Ok(vec![
            CapabilityDescriptor::new(CapabilityId::new("compute.op_a")?, "a", "Does the thing."),
            with_input,
        ])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

struct CompleteAfterObservation;

impl LocalWorkPolicy for CompleteAfterObservation {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        if view.turn == 0 {
            return None;
        }
        view.observations
            .last()
            .filter(|o| o.kind == ObservationKind::ExecutionCompleted)
            .map(|_| WorkDecision::Complete {
                summary: "done".into(),
            })
    }
}

struct Run {
    report: WorkReport,
    model_calls: usize,
    executed: Vec<String>,
    agent: Agent,
}

async fn run(reply: &str) -> Run {
    let model = Arc::new(Model(reply.into(), Mutex::new(0)));
    let exec = Arc::new(Exec(Mutex::new(vec![])));
    let agent = Agent::new(model.clone())
        .with_capabilities(Arc::new(Caps))
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()));
    let spec = WorkSpec::new(WorkId::new("w"), WorkGoal::new("Do the thing."));
    let report = agent
        .run_work(&spec, &CompleteAfterObservation, &ModelDecisionBoundary)
        .await;
    let model_calls = *model.1.lock().unwrap();
    let executed = exec.0.lock().unwrap().clone();
    Run {
        report,
        model_calls,
        executed,
        agent,
    }
}

/// The names of the events that matter to the boundary, in order.
fn shape(report: &WorkReport) -> Vec<&'static str> {
    report
        .events
        .iter()
        .filter_map(|e| match e {
            WorkEvent::ModelCalled { .. } => Some("ModelCalled"),
            WorkEvent::DecisionMade { .. } => Some("DecisionMade"),
            WorkEvent::CapabilityRequested { .. } => Some("CapabilityRequested"),
            WorkEvent::Execution(_) => Some("Execution"),
            WorkEvent::ObservationRecorded { .. } => Some("ObservationRecorded"),
            WorkEvent::EvidenceRecorded { .. } => Some("EvidenceRecorded"),
            WorkEvent::WorkCompleted { .. } => Some("WorkCompleted"),
            WorkEvent::WorkBlocked { .. } => Some("WorkBlocked"),
            WorkEvent::WorkFailed { .. } => Some("WorkFailed"),
            _ => None,
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// A capability was selected and invoked.
    Invoked,
    /// A declared capability was selected, but the invocation was refused.
    InvocationRejected,
    /// No capability was selected: the reply was not a usable decision.
    SelectionRejected,
}

/// Read straight from the trajectory: nothing here parses the model's reply.
fn verdict(report: &WorkReport) -> Verdict {
    let selected = report
        .events
        .iter()
        .any(|e| matches!(e, WorkEvent::CapabilityRequested { .. }));
    let executed = report.summary.executions > 0;
    match (&report.outcome, selected) {
        (_, true) if executed => Verdict::Invoked,
        (WorkOutcome::Blocked { reason }, true)
            if reason.starts_with("invalid capability input") =>
        {
            Verdict::InvocationRejected
        }
        (WorkOutcome::Failed { .. }, false) => Verdict::SelectionRejected,
        other => panic!("unclassifiable trajectory: {other:?}"),
    }
}

fn nothing_happened(r: &Run, reply: &str) {
    assert!(r.executed.is_empty(), "{reply}: executed");
    assert_eq!(r.report.summary.executions, 0, "{reply}");
    assert!(r.report.observations.is_empty(), "{reply}");
    assert!(
        !r.report.events.iter().any(|e| matches!(
            e,
            WorkEvent::Execution(_)
                | WorkEvent::ObservationRecorded { .. }
                | WorkEvent::EvidenceRecorded { .. }
        )),
        "{reply}: {:?}",
        r.report.events
    );
    assert_eq!(r.model_calls, 1, "{reply}: no retry");
    let probe = CapabilityRequest::new(
        ExecutionId::new("p"),
        CapabilityId::new("compute.op_a").unwrap(),
    );
    assert_eq!(
        r.agent.lookup_evidence(&probe),
        EvidenceLookup::NotFound,
        "{reply}"
    );
}

// ---- valid ------------------------------------------------------------------------------

#[tokio::test]
async fn the_one_valid_invocation_is_the_capability_id_alone() {
    let reply = r#"{"schema":"chip.work-decision.v1","decision":"request_capability","capability":"compute.op_a"}"#;
    let r = run(reply).await;
    assert_eq!(r.executed, ["compute.op_a"]);
    assert_eq!(verdict(&r.report), Verdict::Invoked);
    assert_eq!(
        shape(&r.report),
        [
            "ModelCalled",
            "DecisionMade",
            "CapabilityRequested",
            "Execution",
            "Execution",
            "Execution",
            "ObservationRecorded",
            "EvidenceRecorded",
            "DecisionMade",
            "WorkCompleted"
        ]
    );
}

// ---- invocation rejected: a declared capability, invented inputs ----------------------------

#[tokio::test]
async fn invented_inputs_are_an_invocation_failure_after_a_valid_selection() {
    let invented = [
        r#"{"decision":"request_capability","capability":"compute.op_a","inputs":{"test_data":"supplied"}}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a","inputs":{"command":"sha256sum /etc/passwd"}}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a","inputs":{"executable":"/bin/sh"}}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a","inputs":{"input":"fixed payload"}}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a","inputs":{"count":3}}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a","inputs":{"flag":true}}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a","inputs":{"a":"x","b":"y","c":"z"}}"#,
        // Another capability's declared input is not this capability's input.
        r#"{"decision":"request_capability","capability":"compute.op_a","inputs":{"n":3}}"#,
        // Declared input names are not a loophole for undeclared ones next to them.
        r#"{"decision":"request_capability","capability":"compute.op_n","inputs":{"n":3,"command":"rm -rf /"}}"#,
    ];
    for reply in invented {
        let r = run(reply).await;
        // The selection was valid: the model named a declared capability...
        assert_eq!(verdict(&r.report), Verdict::InvocationRejected, "{reply}");
        // ...and the trajectory says so: selected, then refused, and nothing in between.
        assert_eq!(
            shape(&r.report),
            [
                "ModelCalled",
                "DecisionMade",
                "CapabilityRequested",
                "WorkBlocked"
            ],
            "{reply}"
        );
        assert!(
            matches!(&r.report.outcome, WorkOutcome::Blocked { reason }
                if reason.starts_with("invalid capability input: capability does not accept input")),
            "{reply}: {:?}",
            r.report.outcome
        );
        nothing_happened(&r, reply);
    }
}

// ---- selection rejected: nothing usable was selected ----------------------------------------

#[tokio::test]
async fn an_empty_inputs_object_is_an_invented_invocation_like_any_other() {
    // `{}` used to parse as "no inputs" and run. For a capability that declares no inputs the valid
    // shape is the member's absence, and the refusal is the same invocation-stage refusal.
    for reply in [
        r#"{"schema":"chip.work-decision.v1","decision":"request_capability","capability":"compute.op_a","inputs":{}}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a","inputs":{ }}"#,
        "{\"decision\":\"request_capability\",\"capability\":\"compute.op_a\",\"inputs\":{\n}}",
    ] {
        let r = run(reply).await;
        assert_eq!(verdict(&r.report), Verdict::InvocationRejected, "{reply}");
        assert_eq!(
            shape(&r.report),
            [
                "ModelCalled",
                "DecisionMade",
                "CapabilityRequested",
                "WorkBlocked"
            ],
            "{reply}"
        );
        assert!(
            matches!(&r.report.outcome, WorkOutcome::Blocked { reason }
                if reason == "invalid capability input: capability takes no inputs; the request must not carry an `inputs` member"),
            "{reply}: {:?}",
            r.report.outcome
        );
        nothing_happened(&r, reply);
    }
}

#[tokio::test]
async fn the_rule_follows_the_capabilitys_declaration_not_a_blanket_ban() {
    // A capability that declares an input may be sent one, or an empty member (no input given).
    for reply in [
        r#"{"decision":"request_capability","capability":"compute.op_n","inputs":{"n":3}}"#,
        r#"{"decision":"request_capability","capability":"compute.op_n","inputs":{}}"#,
        r#"{"decision":"request_capability","capability":"compute.op_n"}"#,
    ] {
        let r = run(reply).await;
        assert_eq!(verdict(&r.report), Verdict::Invoked, "{reply}");
        assert_eq!(r.executed, ["compute.op_n"], "{reply}");
    }
}

#[tokio::test]
async fn fields_only_the_runtime_may_produce_are_rejected_not_ignored() {
    let forbidden = [
        r#""execution_id":"mine""#,
        r#""receipt":"sha256:forged""#,
        r#""status":"success""#,
        r#""result":"the digest is abc""#,
        r#""observation":"it worked""#,
        r#""executable":"/bin/sh""#,
        r#""command":"sha256sum x""#,
        r#""output":"abc""#,
        r#""evidence":"recorded""#,
        r#""status":"success","receipt":"r-1","result":"ok""#,
    ];
    for field in forbidden {
        let reply = format!(
            r#"{{"schema":"chip.work-decision.v1","decision":"request_capability","capability":"compute.op_a",{field}}}"#
        );
        let r = run(&reply).await;
        assert_eq!(verdict(&r.report), Verdict::SelectionRejected, "{reply}");
        assert_eq!(shape(&r.report), ["ModelCalled", "WorkFailed"], "{reply}");
        nothing_happened(&r, &reply);
    }
}

#[tokio::test]
async fn an_undeclared_or_malformed_selection_is_a_selection_failure() {
    for reply in [
        r#"{"decision":"request_capability","capability":"compute.op_fake"}"#,
        r#"{"decision":"request_capability","capability":"shell.exec"}"#,
        r#"{"decision":"request_capability"}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a"}{"decision":"complete","summary":"x"}"#,
        r#"{"schema":"chip.work-decision.v2","decision":"request_capability","capability":"compute.op_a"}"#,
        "I would run compute.op_a.",
        "",
    ] {
        let r = run(reply).await;
        assert_eq!(verdict(&r.report), Verdict::SelectionRejected, "{reply}");
        assert_eq!(shape(&r.report), ["ModelCalled", "WorkFailed"], "{reply}");
        nothing_happened(&r, reply);
    }
}

#[tokio::test]
async fn the_two_failure_modes_are_told_apart_by_the_trajectory_alone() {
    let selection =
        run(r#"{"decision":"request_capability","capability":"compute.op_fake"}"#).await;
    let invocation = run(
        r#"{"decision":"request_capability","capability":"compute.op_a","inputs":{"test_data":"x"}}"#,
    )
    .await;
    assert_eq!(verdict(&selection.report), Verdict::SelectionRejected);
    assert_eq!(verdict(&invocation.report), Verdict::InvocationRejected);
    // The invocation failure got as far as selecting a declared capability; the other did not.
    assert!(!shape(&selection.report).contains(&"CapabilityRequested"));
    assert!(shape(&invocation.report).contains(&"CapabilityRequested"));
    assert!(matches!(
        selection.report.outcome,
        WorkOutcome::Failed { .. }
    ));
    assert!(matches!(
        invocation.report.outcome,
        WorkOutcome::Blocked { .. }
    ));
}
