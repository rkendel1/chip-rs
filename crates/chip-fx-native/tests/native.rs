//! Chip's model port over the authoritative FX and the native provider, against a mock server.
//!
//! Mocked-transport evidence: it shows how Chip, FX and the native provider behave for these
//! bytes. It says nothing about a live provider or the TLS path.

mod mock;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityProvider,
    ExecutionError, ExecutionRequest, ExecutionResult, Executor, ModelDecisionBoundary,
    NoLocalPolicy, WorkGoal, WorkId, WorkOutcome, WorkSpec,
};
use chip_fx::{CancelSignal, OptionsPolicy};
use chip_fx_native::{NativeModelProvider, NativeSettings, ProviderKind};
use chip_inference::{Delivery, FxError, Message, MessageRole, ModelProvider, ModelRequest};
use mock::{Reply, Server, reply};

const KEY: &str = "sk-chip-native-0123456789abcdef";

fn settings(server: &Server, options: OptionsPolicy) -> NativeSettings {
    NativeSettings {
        kind: ProviderKind::OpenAiCompatible,
        base_url: server.base_url.clone(),
        api_key: Some(KEY.to_owned()),
        headers: vec![],
        options,
        budget: None,
        default_max_tokens: None,
        id_seed: *b"chip-nat",
    }
}

fn request() -> ModelRequest {
    ModelRequest::new(
        "mock-1",
        vec![
            Message::new(MessageRole::System, "be brief"),
            Message::new(MessageRole::User, "say hi"),
        ],
    )
}

fn failure(error: FxError) -> Box<chip_inference::InferenceFailure> {
    match error {
        FxError::Inference(failure) => failure,
        other => panic!("expected an inference failure, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_completion_goes_chip_to_fx_to_the_native_provider_and_back() {
    let server = Server::start(vec![reply("hello there")]);
    let provider =
        NativeModelProvider::new(settings(&server, OptionsPolicy::DropUnsupported)).unwrap();
    let response = provider.complete(request()).await.unwrap();
    assert_eq!(response.output, "hello there");
    assert_eq!(response.usage.prompt_tokens, 11);
    assert_eq!(response.usage.completion_tokens, 4);
    assert_eq!(server.connections(), 1, "one FX attempt, one provider call");
    assert_eq!(provider.stats().attempts, 1);
    let (head, body) = &server.recorded()[0];
    assert!(head.to_ascii_lowercase().contains(&format!(
        "authorization: bearer {}",
        KEY.to_ascii_lowercase()
    )));
    assert!(!body.contains(KEY), "the credential is only in the header");
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["model"], "mock-1");
    assert_eq!(body["messages"][1]["content"], "say hi");
    assert!(body.get("max_tokens").is_none() && body.get("temperature").is_none());
    assert_eq!(provider.stats().options_dropped, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_http_error_keeps_status_retry_after_and_delivery_and_is_not_retried() {
    let server = Server::start(vec![
        Reply::Status(
            429,
            vec![("retry-after", "7".into())],
            r#"{"error":{"message":"slow down"}}"#.into(),
        ),
        reply("a retry would succeed"),
    ]);
    let provider =
        NativeModelProvider::new(settings(&server, OptionsPolicy::DropUnsupported)).unwrap();
    let f = failure(provider.complete(request()).await.unwrap_err());
    assert_eq!(f.kind, "rate_limited");
    assert_eq!(f.status, Some(429));
    assert_eq!(f.retry_after_ms, Some(7000));
    assert_eq!(f.delivery, Delivery::PossiblySent);
    assert_eq!(server.connections(), 1, "no hidden retry");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_credential_echoed_by_the_provider_never_reaches_the_error() {
    let body = format!(r#"{{"error":{{"message":"Incorrect API key provided: {KEY}"}}}}"#);
    let server = Server::start(vec![Reply::Status(401, vec![], body)]);
    let provider =
        NativeModelProvider::new(settings(&server, OptionsPolicy::DropUnsupported)).unwrap();
    let error = provider.complete(request()).await.unwrap_err();
    let shown = format!("{error} {error:?}");
    assert!(
        !shown.contains(KEY) && !shown.contains("0123456789abcdef"),
        "{shown}"
    );
    assert_eq!(failure(error).kind, "unauthenticated");
}

#[tokio::test(flavor = "multi_thread")]
async fn options_fx_cannot_carry_are_refused_before_any_connection_when_the_host_says_refuse() {
    let server = Server::start(vec![]);
    let provider = NativeModelProvider::new(settings(&server, OptionsPolicy::Refuse)).unwrap();
    let error = provider.complete(request()).await.unwrap_err();
    assert!(
        matches!(&error, FxError::InvalidRequest(_))
            || matches!(&error, FxError::Inference(f) if f.delivery == Delivery::DefinitelyUnsent),
        "{error:?}"
    );
    assert_eq!(server.connections(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_attempt_budget_ends_a_silent_server_as_a_timeout_that_may_have_been_sent() {
    let server = Server::start(vec![Reply::Silent]);
    let mut s = settings(&server, OptionsPolicy::DropUnsupported);
    s.budget = Some(Duration::from_millis(300));
    let provider = NativeModelProvider::new(s).unwrap();
    let f = failure(provider.complete(request()).await.unwrap_err());
    assert_eq!(f.kind, "timeout");
    assert_eq!(f.delivery, Delivery::PossiblySent);
    assert_eq!(server.connections(), 1);
}

struct After(Arc<AtomicBool>);

impl CancelSignal for After {
    fn cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_mid_stream_stops_the_attempt_and_keeps_the_partial_output() {
    let server = Server::start(vec![Reply::Stall(vec![mock::delta("so far")])]);
    let flag = Arc::new(AtomicBool::new(false));
    let provider = NativeModelProvider::with_cancel(
        settings(&server, OptionsPolicy::DropUnsupported),
        Arc::new(After(flag.clone())),
    )
    .unwrap();
    let setter = flag.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        setter.store(true, Ordering::SeqCst);
    });
    let started = std::time::Instant::now();
    let f = failure(provider.complete(request()).await.unwrap_err());
    assert_eq!(f.kind, "cancelled");
    assert_eq!(f.partial_output.as_deref(), Some("so far"));
    assert_eq!(f.delivery, Delivery::ConfirmedResponse);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(server.connections(), 1);
}

#[test]
fn settings_the_provider_refuses_are_configuration_errors_and_never_print_the_key() {
    let mut s = NativeSettings {
        kind: ProviderKind::OpenAiCompatible,
        base_url: "http://api.example.test/v1".into(),
        api_key: Some(KEY.into()),
        headers: vec![],
        options: OptionsPolicy::DropUnsupported,
        budget: None,
        default_max_tokens: None,
        id_seed: [0; 8],
    };
    assert!(matches!(
        NativeModelProvider::new(s.clone()),
        Err(FxError::Configuration(_))
    ));
    s.base_url = "https://user:pw@api.example.test/v1".into();
    assert!(matches!(
        NativeModelProvider::new(s.clone()),
        Err(FxError::Configuration(_))
    ));
    assert!(!format!("{s:?}").contains(KEY));
}

// ---- the real Chip work loop over the native provider ----------------------------------------

struct Caps;

#[async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(vec![])
    }
    async fn availability(&self, _: &chip_core::CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

struct Nothing;

#[async_trait]
impl Executor for Nothing {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        Ok(ExecutionResult::success(request.id, "ran"))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_chip_work_loop_completes_over_the_native_provider_with_one_provider_call() {
    let server = Server::start(vec![reply(
        r#"{"decision":"complete","summary":"done natively"}"#,
    )]);
    let provider = Arc::new(
        NativeModelProvider::new(settings(&server, OptionsPolicy::DropUnsupported)).unwrap(),
    );
    let agent = Agent::new(provider.clone())
        .with_capabilities(Arc::new(Caps))
        .with_executor(Arc::new(Nothing))
        .with_observer(Arc::new(chip_core::ExecutionObserver));
    let spec = WorkSpec::new(WorkId::new("w"), WorkGoal::new("say done"));
    let report = agent
        .run_work(&spec, &NoLocalPolicy, &ModelDecisionBoundary)
        .await;
    assert!(
        matches!(&report.outcome, WorkOutcome::Completed { summary } if summary == "done natively"),
        "{:?}",
        report.outcome
    );
    assert_eq!(server.connections(), 1);
    assert_eq!(provider.stats().attempts, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_attempt_ends_the_work_without_a_second_provider_call() {
    let server = Server::start(vec![
        Reply::Status(500, vec![], r#"{"error":{"message":"boom"}}"#.into()),
        reply(r#"{"decision":"complete","summary":"never"}"#),
    ]);
    let provider = Arc::new(
        NativeModelProvider::new(settings(&server, OptionsPolicy::DropUnsupported)).unwrap(),
    );
    let agent = Agent::new(provider)
        .with_capabilities(Arc::new(Caps))
        .with_executor(Arc::new(Nothing))
        .with_observer(Arc::new(chip_core::ExecutionObserver));
    let spec = WorkSpec::new(WorkId::new("w"), WorkGoal::new("say done"));
    let report = agent
        .run_work(&spec, &NoLocalPolicy, &ModelDecisionBoundary)
        .await;
    assert!(
        matches!(report.outcome, WorkOutcome::Failed { .. }),
        "{:?}",
        report.outcome
    );
    assert_eq!(
        server.connections(),
        1,
        "Chip does not retry and FX does not either"
    );
}
