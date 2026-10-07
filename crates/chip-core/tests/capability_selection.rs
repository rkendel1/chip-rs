//! PR31: the model chooses among declared capabilities; Chip validates, executes and observes.
//!
//! Deterministic and offline: a scripted model reply goes through the real
//! `ModelDecisionBoundary`, and a recording executor stands in for Compute. The model's text never
//! becomes an execution, an observation or evidence; only the executor's result can.

use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, CapabilityRequest, EvidenceLookup, ExecutionError, ExecutionId,
    ExecutionObserver, ExecutionRequest, ExecutionResult, Executor, LocalWorkPolicy,
    ModelDecisionBoundary, ObservationKind, TestLocalReasoner, WorkDecision, WorkEvent, WorkGoal,
    WorkId, WorkOutcome, WorkReport, WorkSpec, WorkView,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

/// SHA-256 of the fixed test input: known here independently of the executor and of any model.
const DIGEST: &str = "5dc3ef3419c119833659d7288d246c87db93e4eeabb2b11126be581cb83c165f";

/// A declared capability set and which of its members answers the goal "digest of the fixed
/// input". `unavailable` is declared but not usable, so it is never offered to the model.
struct Set {
    name: &'static str,
    capabilities: &'static [(&'static str, &'static str)],
    unavailable: (&'static str, &'static str),
    /// The capability whose execution produces the digest.
    answers: &'static str,
}

/// PR31: ids that suggest what they do.
const NAMED: Set = Set {
    name: "named",
    capabilities: &[
        ("compute.hash", "SHA-256 digest of a fixed text"),
        ("compute.selftest", "Deterministic Compute execution test"),
        ("compute.system_info", "Report the runtime version"),
    ],
    unavailable: ("compute.offline", "Not usable right now"),
    answers: "compute.hash",
};

/// PR32: ids that say nothing; only the descriptions carry the meaning.
const OPAQUE: Set = Set {
    name: "opaque",
    capabilities: &[
        (
            "compute.op_a",
            "Produce a SHA-256 digest of the fixed test input.",
        ),
        (
            "compute.op_b",
            "Report deterministic information about the Compute runtime.",
        ),
        (
            "compute.op_c",
            "Run the existing Compute self-test and report its result.",
        ),
    ],
    unavailable: ("compute.op_d", "Not usable right now"),
    answers: "compute.op_a",
};

const HASH_TEXT: &str = "Produce a SHA-256 digest of the fixed test input.";
const INFO_TEXT: &str = "Report deterministic information about the Compute runtime.";
const SELFTEST_TEXT: &str = "Run the existing Compute self-test and report its result.";

/// PR33, mapping 2: op_a is the runtime report, op_b the digest, op_c the self test.
const MAPPING_B: Set = Set {
    name: "mapping b",
    capabilities: &[
        ("compute.op_a", INFO_TEXT),
        ("compute.op_b", HASH_TEXT),
        ("compute.op_c", SELFTEST_TEXT),
    ],
    unavailable: ("compute.op_d", "Not usable right now"),
    answers: "compute.op_b",
};

/// PR33, mapping 3: op_a is the self test, op_b the runtime report, op_c the digest.
const MAPPING_C: Set = Set {
    name: "mapping c",
    capabilities: &[
        ("compute.op_a", SELFTEST_TEXT),
        ("compute.op_b", INFO_TEXT),
        ("compute.op_c", HASH_TEXT),
    ],
    unavailable: ("compute.op_d", "Not usable right now"),
    answers: "compute.op_c",
};

/// PR33: mapping 2 presented in another order, so neither the id nor the position gives it away.
const DEALT: Set = Set {
    name: "dealt",
    capabilities: &[
        ("compute.op_c", SELFTEST_TEXT),
        ("compute.op_a", INFO_TEXT),
        ("compute.op_b", HASH_TEXT),
    ],
    unavailable: ("compute.op_d", "Not usable right now"),
    answers: "compute.op_b",
};

/// Every opaque dealing; `OPAQUE` above is mapping 1.
const OPAQUE_SETS: [&Set; 4] = [&OPAQUE, &MAPPING_B, &MAPPING_C, &DEALT];
const ALL_SETS: [&Set; 5] = [&NAMED, &OPAQUE, &MAPPING_B, &MAPPING_C, &DEALT];

/// Replies with fixed text and keeps what it was sent.
struct Model {
    reply: String,
    sent: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ModelProvider for Model {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        self.sent.lock().unwrap().push(
            request
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect(),
        );
        Ok(ModelResponse::new(
            "msg_01Abc",
            self.reply.clone(),
            Usage::new(5, 5),
        ))
    }
}

/// Records every request it is handed; the answer is the executor's own, never the model's. Only
/// the capability that answers the goal produces the digest.
struct Exec {
    calls: Mutex<Vec<(String, String)>>,
    answers: &'static str,
}

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.calls
            .lock()
            .unwrap()
            .push((r.id.to_string(), r.intent.clone()));
        let output = if r.intent == self.answers {
            DIGEST.to_string()
        } else {
            format!("RESULT-OF-{}", r.intent)
        };
        Ok(ExecutionResult::success(r.id, output).with_receipt_id("sha256:from-the-executor"))
    }
}

struct Caps(&'static Set);

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let d = |(id, text): (&str, &str)| -> Result<_, CapabilityError> {
            Ok(CapabilityDescriptor::new(CapabilityId::new(id)?, id, text))
        };
        let mut all = self
            .0
            .capabilities
            .iter()
            .map(|c| d(*c))
            .collect::<Result<Vec<_>, _>>()?;
        all.push(d(self.0.unavailable)?);
        Ok(all)
    }

    async fn availability(&self, id: &CapabilityId) -> CapabilityAvailability {
        if id.as_str() == self.0.unavailable.0 {
            CapabilityAvailability::Unavailable("down".into())
        } else {
            CapabilityAvailability::Available
        }
    }
}

/// Ask the model first; afterwards complete with what the executor observed.
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
                summary: format!("observed: {}", o.output.clone().unwrap_or_default()),
            })
    }
}

struct Run {
    report: WorkReport,
    model: Arc<Model>,
    exec: Arc<Exec>,
    agent: Agent,
}

const DEFAULT_GOAL: &str = "Determine the SHA-256 digest of the fixed test input.";

/// PR35: three wordings of the same goal that share no vocabulary with the capability description.
const GOALS: [&str; 3] = [
    "Produce the canonical fingerprint of the test payload.",
    "Calculate the deterministic identity value for the supplied test data.",
    "Return the standard cryptographic representation of the fixed payload.",
];

async fn run(set: &'static Set, reply: &str) -> Run {
    run_goal(set, DEFAULT_GOAL, reply).await
}

async fn run_goal(set: &'static Set, goal: &str, reply: &str) -> Run {
    let model = Arc::new(Model {
        reply: reply.into(),
        sent: Mutex::new(vec![]),
    });
    let exec = Arc::new(Exec {
        calls: Mutex::new(vec![]),
        answers: set.answers,
    });
    let agent = Agent::new(model.clone())
        .with_capabilities(Arc::new(Caps(set)))
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()));
    let spec = WorkSpec::new(WorkId::new("w"), WorkGoal::new(goal));
    let report = agent
        .run_work(&spec, &ReportObservation, &ModelDecisionBoundary)
        .await;
    Run {
        report,
        model,
        exec,
        agent,
    }
}

fn lookup(run: &Run, capability: &str) -> EvidenceLookup {
    run.agent.lookup_evidence(&CapabilityRequest::new(
        ExecutionId::new("probe"),
        CapabilityId::new(capability).unwrap(),
    ))
}

fn any_reality(report: &WorkReport) -> bool {
    report.events.iter().any(|e| {
        matches!(
            e,
            WorkEvent::Execution(_)
                | WorkEvent::ObservationRecorded { .. }
                | WorkEvent::EvidenceRecorded { .. }
        )
    })
}

fn request(capability: &str) -> String {
    format!(
        r#"{{"schema":"chip.work-decision.v1","decision":"request_capability","capability":"{capability}"}}"#
    )
}

/// Whether the work found the digest: decided from the executor's observation alone.
fn satisfied(report: &WorkReport) -> bool {
    report.observations.iter().any(|o| {
        o.kind == ObservationKind::ExecutionCompleted && o.output.as_deref() == Some(DIGEST)
    }) && matches!(&report.outcome, WorkOutcome::Completed { summary } if summary.contains(DIGEST))
}

// ---- the matrix: declared choices execute -------------------------------------------------

#[tokio::test]
async fn every_declared_capability_the_model_chooses_executes_through_the_executor() {
    for set in ALL_SETS {
        for (id, _) in set.capabilities {
            let r = run(set, &request(id)).await;
            let name = format!("{}/{id}", set.name);
            let calls = r.exec.calls.lock().unwrap().clone();
            assert_eq!(calls.len(), 1, "{name}");
            assert_eq!(
                calls[0].1, *id,
                "{name}: the executor is asked for exactly that"
            );
            // Chip named the execution; the model's text did not.
            assert!(calls[0].0.starts_with("model-"), "{name}: {}", calls[0].0);

            assert_eq!(
                r.model.sent.lock().unwrap().len(),
                1,
                "{name}: one model call"
            );
            assert_eq!(r.report.observations.len(), 1, "{name}");
            assert_eq!(
                r.report.observations[0].receipt_id.as_deref(),
                Some("sha256:from-the-executor")
            );
            assert!(
                r.report
                    .events
                    .iter()
                    .any(|e| matches!(e, WorkEvent::EvidenceRecorded { .. })),
                "{name}"
            );
            assert!(
                matches!(lookup(&r, id), EvidenceLookup::Found(o) if o.output == r.report.observations[0].output),
                "{name}"
            );
            // The work completes either way; whether the goal was met is the observation's say-so.
            assert!(
                matches!(r.report.outcome, WorkOutcome::Completed { .. }),
                "{name}"
            );
            assert_eq!(satisfied(&r.report), *id == set.answers, "{name}");
        }
    }
}

#[tokio::test]
async fn a_valid_but_wrong_capability_runs_and_does_not_answer_the_goal() {
    // Chip does not pretend every valid capability is the right one: each declared capability
    // other than the digest executes for real, and the observation shows the digest was not
    // produced. Which one is right depends on the dealing.
    for set in OPAQUE_SETS {
        for (id, _) in set.capabilities {
            let r = run(set, &request(id)).await;
            let name = format!("{}/{id}", set.name);
            assert_eq!(r.report.summary.executions, 1, "{name}");
            assert_eq!(r.report.observations.len(), 1, "{name}");
            if *id == set.answers {
                assert_eq!(r.report.observations[0].output.as_deref(), Some(DIGEST));
                assert!(satisfied(&r.report), "{name}");
            } else {
                assert_ne!(
                    r.report.observations[0].output.as_deref(),
                    Some(DIGEST),
                    "{name}"
                );
                assert!(!satisfied(&r.report), "{name}");
            }
        }
    }
}

#[test]
fn the_right_capability_changes_with_the_mapping_and_always_carries_the_digest_description() {
    let answers: Vec<&str> = OPAQUE_SETS.iter().map(|s| s.answers).collect();
    // Mappings 1-3 put the digest on a, b and c in turn; the dealt set repeats b.
    assert_eq!(
        answers,
        [
            "compute.op_a",
            "compute.op_b",
            "compute.op_c",
            "compute.op_b"
        ]
    );
    for set in OPAQUE_SETS {
        let described: Vec<&str> = set
            .capabilities
            .iter()
            .filter(|(_, d)| *d == HASH_TEXT)
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(described, [set.answers], "{}", set.name);
        // Same three ids and descriptions every time: only the pairing and the order move.
        let mut ids: Vec<&str> = set.capabilities.iter().map(|(i, _)| *i).collect();
        ids.sort();
        assert_eq!(ids, ["compute.op_a", "compute.op_b", "compute.op_c"]);
    }
}

// ---- the matrix: everything else is rejected before anything runs ---------------------------

fn rejected_replies(valid: &str) -> Vec<(&'static str, String)> {
    let field = |extra: &str| {
        format!(r#"{{"decision":"request_capability","capability":"{valid}",{extra}}}"#)
    };
    vec![
        ("undeclared capability", request("compute.fake")),
        // The obvious name is not declared in the opaque set, and guessing it gets nothing.
        ("a guessed, undeclared name", request("compute.sha256")),
        (
            "missing capability",
            r#"{"decision":"request_capability"}"#.into(),
        ),
        ("prose claim", "The hash is abc123.".into()),
        (
            "prose claiming the digest",
            format!("The SHA-256 digest is {DIGEST}."),
        ),
        (
            "prose around the object",
            format!("Sure! {}", request(valid)),
        ),
        (
            "two objects, the second a completion",
            format!(
                r#"{}{{"decision":"complete","summary":"The hash is abc123"}}"#,
                request(valid)
            ),
        ),
        (
            "completion fields on a request",
            field(r#""summary":"The hash is abc123""#),
        ),
        (
            "model-supplied execution id",
            field(r#""execution_id":"mine""#),
        ),
        (
            "model-supplied command",
            field(r#""command":"sha256sum /etc/passwd""#),
        ),
        (
            "model-supplied executable",
            field(r#""executable":"/bin/sh""#),
        ),
        (
            "model-supplied receipt and status",
            field(r#""status":"success","receipt":"r-1""#),
        ),
        (
            "an input the capability never declared",
            field(r#""inputs":{"text":"anything"}"#),
        ),
        (
            "wrong schema version",
            format!(
                r#"{{"schema":"chip.work-decision.v2","decision":"request_capability","capability":"{valid}"}}"#
            ),
        ),
    ]
}

#[tokio::test]
async fn undeclared_malformed_and_smuggling_replies_execute_nothing() {
    for set in ALL_SETS {
        let mut rejected = rejected_replies(set.answers);
        rejected.push(("declared but unavailable", request(set.unavailable.0)));
        for (what, reply) in rejected {
            let name = format!("{}: {what}", set.name);
            let r = run(set, &reply).await;
            assert!(
                !matches!(r.report.outcome, WorkOutcome::Completed { .. }),
                "{name}: {:?}",
                r.report.outcome
            );
            assert!(r.exec.calls.lock().unwrap().is_empty(), "{name}: executed");
            assert!(r.report.observations.is_empty(), "{name}");
            assert!(!any_reality(&r.report), "{name}: {:?}", r.report.events);
            assert_eq!(r.model.sent.lock().unwrap().len(), 1, "{name}: no retry");
            assert!(!satisfied(&r.report), "{name}");
            for (id, _) in set.capabilities {
                assert_eq!(lookup(&r, id), EvidenceLookup::NotFound, "{name}: {id}");
            }
        }
    }
}

#[tokio::test]
async fn the_obvious_name_is_not_a_capability_in_the_opaque_set() {
    // The PR31 ids do not exist here, so a model that answers from the goal's words alone
    // ("hash") instead of from the declared capabilities executes nothing.
    for set in OPAQUE_SETS {
        for guess in ["compute.hash", "compute.selftest", "compute.system_info"] {
            let r = run(set, &request(guess)).await;
            assert!(r.exec.calls.lock().unwrap().is_empty(), "{guess}");
            assert!(!any_reality(&r.report), "{guess}");
        }
    }
}

#[tokio::test]
async fn a_completion_claim_without_execution_is_not_evidence() {
    // A bare completion is a legal decision under the contract, so the work ends; but the claim in
    // it is only the model's words: nothing ran, nothing was observed, nothing was recorded.
    for set in ALL_SETS {
        let claim = format!(r#"{{"decision":"complete","summary":"The digest is {DIGEST}"}}"#);
        let r = run(set, &claim).await;
        assert_eq!(
            r.report.outcome,
            WorkOutcome::Completed {
                summary: format!("The digest is {DIGEST}")
            }
        );
        assert!(r.exec.calls.lock().unwrap().is_empty());
        assert!(r.report.observations.is_empty());
        assert_eq!(r.report.summary.executions, 0);
        assert!(!any_reality(&r.report));
        assert_eq!(r.agent.evidence_stats().hits, 0);
        for (id, _) in set.capabilities {
            assert_eq!(lookup(&r, id), EvidenceLookup::NotFound);
        }
        // Even though the model's words contain the right digest, nothing counts them as one.
        assert!(!satisfied(&r.report), "{}", set.name);
    }
}

// ---- the escalation context exposes what can be chosen ---------------------------------------

#[tokio::test]
async fn the_model_is_told_the_available_capabilities_and_what_each_does() {
    for set in ALL_SETS {
        let r = run(set, &request(set.answers)).await;
        let sent = r.model.sent.lock().unwrap()[0].clone();
        assert!(
            sent.contains("Goal: Determine the SHA-256 digest of the fixed test input."),
            "{}",
            set.name
        );
        for (id, text) in set.capabilities {
            assert!(sent.contains(&format!("{id} - {text}")), "{id}: {sent}");
        }
        assert!(
            !sent.contains(set.unavailable.0) && !sent.contains(set.unavailable.1),
            "{}: unavailable is not offered",
            set.name
        );
        // The model is told the shape of the only thing it may return, and nothing it could run.
        assert!(sent.contains("chip.work-decision.v1"));
    }
}

#[tokio::test]
async fn the_opaque_context_names_what_each_capability_does_not_what_it_is() {
    for set in OPAQUE_SETS {
        let r = run(set, &request(set.answers)).await;
        let sent = r.model.sent.lock().unwrap()[0].clone();
        let offered = sent.split("Available capabilities:").nth(1).unwrap();
        // Mechanism and answer stay out of what the model reads.
        for detail in ["hashlib", "platform.", "print(", "python", ".py", DIGEST] {
            assert!(
                !offered.contains(detail),
                "{}: the context leaks {detail}",
                set.name
            );
        }
        for (id, _) in set.capabilities {
            assert!(offered.contains(id));
            for word in ["hash", "sha", "digest", "self", "test", "info"] {
                assert!(!id.contains(word), "{id} reveals '{word}'");
            }
        }
    }
}

#[tokio::test]
async fn capabilities_are_presented_in_the_order_they_were_declared() {
    for set in ALL_SETS {
        let r = run(set, &request(set.answers)).await;
        let sent = r.model.sent.lock().unwrap()[0].clone();
        let offered = sent.split("Available capabilities: ").nth(1).unwrap();
        let positions: Vec<usize> = set
            .capabilities
            .iter()
            .map(|(id, text)| offered.find(&format!("{id} - {text}")).unwrap())
            .collect();
        let mut sorted = positions.clone();
        sorted.sort();
        assert_eq!(
            positions, sorted,
            "{}: declared order is presentation order",
            set.name
        );
    }
    // The dealt set reads op_c first: not the id order, which would have put op_a first.
    let r = run(&DEALT, &request(DEALT.answers)).await;
    let sent = r.model.sent.lock().unwrap()[0].clone();
    assert!(sent.find("compute.op_c").unwrap() < sent.find("compute.op_a").unwrap());
}

// ---- PR35: the goal is worded without the capability's vocabulary -----------------------------

#[test]
fn the_goal_wordings_do_not_repeat_the_descriptions_vocabulary() {
    for goal in GOALS {
        let lower = goal.to_lowercase();
        for word in ["sha", "digest", "hash", "fixed test input", "256"] {
            assert!(!lower.contains(word), "'{goal}' contains '{word}'");
        }
        // Nor do they name any of the three operations by their own words.
        for word in ["self-test", "selftest", "runtime", "compute"] {
            assert!(!lower.contains(word), "'{goal}' contains '{word}'");
        }
    }
}

#[tokio::test]
async fn the_same_mapping_expects_the_same_capability_whatever_the_goal_wording() {
    for set in OPAQUE_SETS {
        for goal in GOALS {
            let name = format!("{} / {goal}", set.name);
            let r = run_goal(set, goal, &request(set.answers)).await;
            // The model was asked the reworded goal...
            let sent = r.model.sent.lock().unwrap()[0].clone();
            assert!(sent.contains(&format!("Goal: {goal}")), "{name}");
            // ...the capability that answers it is the mapping's, not the wording's...
            let calls = r.exec.calls.lock().unwrap().clone();
            assert_eq!(calls.len(), 1, "{name}");
            assert_eq!(calls[0].1, set.answers, "{name}");
            // ...and it is the observation that shows the goal was met.
            assert!(satisfied(&r.report), "{name}");
        }
    }
}

#[tokio::test]
async fn a_wrong_but_valid_choice_executes_and_reality_shows_the_goal_unmet() {
    for set in OPAQUE_SETS {
        for goal in GOALS {
            for (wrong, _) in set.capabilities.iter().filter(|(id, _)| *id != set.answers) {
                let name = format!("{} / {goal} / {wrong}", set.name);
                let r = run_goal(set, goal, &request(wrong)).await;

                // Valid, so Chip does not reject it: it executes for real.
                assert_eq!(r.exec.calls.lock().unwrap().len(), 1, "{name}");
                assert_eq!(r.report.summary.executions, 1, "{name}");
                // The observation is recorded, and it is not the fingerprint.
                assert_eq!(r.report.observations.len(), 1, "{name}");
                assert_eq!(
                    r.report.observations[0].kind,
                    ObservationKind::ExecutionCompleted,
                    "{name}"
                );
                assert_ne!(
                    r.report.observations[0].output.as_deref(),
                    Some(DIGEST),
                    "{name}"
                );
                // Evidence exists only for what ran, and none of it claims the fingerprint.
                let recorded: Vec<String> = r
                    .report
                    .events
                    .iter()
                    .filter_map(|e| match e {
                        WorkEvent::EvidenceRecorded { capability, .. } => {
                            Some(capability.to_string())
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(recorded, [wrong.to_string()], "{name}");
                assert!(
                    matches!(lookup(&r, wrong), EvidenceLookup::Found(o) if o.output.as_deref() != Some(DIGEST)),
                    "{name}"
                );
                assert_eq!(lookup(&r, set.answers), EvidenceLookup::NotFound, "{name}");
                // The goal is not met; nothing treats the completed execution as the answer.
                assert!(!satisfied(&r.report), "{name}");
            }
        }
    }
}
