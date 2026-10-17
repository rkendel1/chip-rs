//! Chip over the authoritative FX, end to end, offline.
//!
//! These tests run the real Chip work loop (`chip-core`), the real model port adapter
//! (`chip-fx`) and the real FX executor (`fx-core` with `fx-single-attempt`). The only
//! doubles sit where a host sits: the provider port under FX and the capability executor under
//! Chip. They prove the path Caller -> Chip -> FX -> provider -> FX report -> Chip.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chip_core::{
    Agent, Delivery, ModelDecisionBoundary, NoLocalPolicy, WorkEvent, WorkOutcome, WorkReport,
    verify_trajectory,
};
use chip_fx::{CancelSignal, FxModelProvider, OptionsPolicy, RequestIds, with_call_cancel};
use common::{
    Attempt, COMPLETE, Caps, CountingExecutor, REQUEST_SELFTEST, SequencePort, failure, spec,
};
use fx_core::{Part, Request, Role};
use fx_single_attempt::{Observed, PortFailureKind, PortSupport};

type Shared<T> = Arc<std::sync::Mutex<T>>;

struct Harness {
    agent: Agent,
    provider: Arc<FxModelProvider<SequencePort>>,
    executor: Arc<CountingExecutor>,
    requests: Shared<Vec<Request>>,
    invokes: Arc<std::sync::atomic::AtomicUsize>,
}

fn harness_with(
    port: SequencePort,
    executor: Arc<CountingExecutor>,
    policy: OptionsPolicy,
    cancel: Option<Arc<dyn CancelSignal>>,
) -> Harness {
    let requests = port.requests.clone();
    let invokes = port.invokes.clone();
    let mut provider = FxModelProvider::new(port, RequestIds::new(*b"chip-fx\0"), policy);
    if let Some(cancel) = cancel {
        provider = provider.with_cancel(cancel);
    }
    let provider = Arc::new(provider);
    let agent = Agent::new(provider.clone())
        .with_capabilities(Arc::new(Caps))
        .with_executor(executor.clone())
        .with_observer(Arc::new(chip_core::ExecutionObserver));
    Harness {
        agent,
        provider,
        executor,
        requests,
        invokes,
    }
}

fn harness(attempts: Vec<Attempt>) -> Harness {
    harness_with(
        SequencePort::new(attempts),
        CountingExecutor::new(),
        OptionsPolicy::DropUnsupported,
        None,
    )
}

async fn run(h: &Harness) -> WorkReport {
    h.agent
        .run_work(&spec(), &NoLocalPolicy, &ModelDecisionBoundary)
        .await
}

fn reason(report: &WorkReport) -> &str {
    match &report.outcome {
        WorkOutcome::Failed { reason, .. }
        | WorkOutcome::Blocked { reason }
        | WorkOutcome::Escalated { reason } => reason,
        other => panic!("expected a non-success outcome with a reason, got {other:?}"),
    }
}

/// The typed delivery certainty on a failed work (G5): what a caller reads instead of `reason`.
fn delivery(report: &WorkReport) -> Option<Delivery> {
    match &report.outcome {
        WorkOutcome::Failed { delivery, .. } => *delivery,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn texts(request: &Request) -> Vec<(Role, String)> {
    request
        .messages
        .iter()
        .map(|message| {
            let text = message
                .parts
                .iter()
                .filter_map(|part| match part {
                    Part::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            (message.role, text)
        })
        .collect()
}

// ---------------------------------------------------------------- agent execution

#[tokio::test]
async fn a_single_step_request_completes_through_the_authoritative_fx() {
    let h = harness(vec![Attempt::Reply(COMPLETE.into())]);
    let report = run(&h).await;
    assert!(
        matches!(&report.outcome, WorkOutcome::Completed { summary } if summary == "the self test passed"),
        "{:?}",
        report.outcome
    );
    assert_eq!(
        h.invokes.load(Ordering::SeqCst),
        1,
        "one FX attempt, one provider call"
    );
    assert_eq!(h.executor.runs(), 0, "a plain completion executes nothing");
    assert_eq!(h.provider.stats().attempts, 1);
    assert_eq!(
        verify_trajectory(&report.events, &spec().limits),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn a_multi_step_turn_runs_the_tool_and_feeds_the_result_to_the_next_inference() {
    let h = harness(vec![
        Attempt::Reply(REQUEST_SELFTEST.into()),
        Attempt::Reply(COMPLETE.into()),
    ]);
    let report = run(&h).await;
    assert!(
        matches!(report.outcome, WorkOutcome::Completed { .. }),
        "{:?}",
        report.outcome
    );
    assert_eq!(
        h.invokes.load(Ordering::SeqCst),
        2,
        "two inference attempts"
    );
    assert_eq!(
        h.executor.runs(),
        1,
        "Chip executed the authorized capability once"
    );

    let requests = h.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let first = texts(&requests[0]);
    let second = texts(&requests[1]);
    assert!(
        !first
            .iter()
            .any(|(_, text)| text.contains("self test passed")),
        "the first inference cannot know the result"
    );
    assert!(
        second
            .iter()
            .any(|(role, text)| *role == Role::System && text.contains("self test passed")),
        "the second inference carries the tool result as an observation: {second:?}"
    );
    // Each attempt has its own request id.
    assert_ne!(
        requests[0].correlation.request_id,
        requests[1].correlation.request_id
    );
}

#[tokio::test]
async fn the_inference_request_is_text_only_and_carries_no_tools() {
    let h = harness(vec![Attempt::Reply(COMPLETE.into())]);
    run(&h).await;
    let requests = h.requests.lock().unwrap();
    let request = &requests[0];
    assert!(request.tools.is_empty(), "Chip proposes no provider tools");
    assert!(
        request
            .messages
            .iter()
            .all(|m| m.parts.iter().all(|p| matches!(p, Part::Text(_))))
    );
    assert!(request.options.max_output_tokens.is_none());
    assert_eq!(h.provider.stats().options_dropped, 1);
    let (role, text) = texts(request).pop().unwrap();
    assert_eq!(role, Role::User);
    assert!(
        text.contains("Run the self test"),
        "the goal reaches the model: {text}"
    );
}

// ---------------------------------------------------------------- tool policy

#[tokio::test]
async fn an_unauthorized_proposal_is_rejected_and_nothing_executes() {
    for reply in [
        r#"{"decision":"request_capability","capability":"deploy.production"}"#,
        r#"{"decision":"request_capability","capability":"rm -rf /"}"#,
        r#"{"decision":"request_capability","capability":"compute.selftest","command":"rm -rf /"}"#,
    ] {
        let h = harness(vec![Attempt::Reply(reply.into())]);
        let report = run(&h).await;
        assert!(
            !matches!(report.outcome, WorkOutcome::Completed { .. }),
            "{reply}: {:?}",
            report.outcome
        );
        assert_eq!(
            h.executor.runs(),
            0,
            "{reply}: the proposal executed nothing"
        );
        assert_eq!(
            h.invokes.load(Ordering::SeqCst),
            1,
            "{reply}: no second inference to repair it"
        );
    }
}

#[tokio::test]
async fn a_malformed_proposal_is_rejected_safely_without_a_retry() {
    for reply in [
        "not json at all",
        "",
        "Run the self test now:\n```sh\ncompute selftest\n```",
    ] {
        let h = harness(vec![Attempt::Reply(reply.into())]);
        let report = run(&h).await;
        assert!(
            matches!(report.outcome, WorkOutcome::Failed { .. }),
            "{reply}: {:?}",
            report.outcome
        );
        assert!(
            reason(&report).contains("not a valid decision"),
            "{}",
            reason(&report)
        );
        assert_eq!(h.executor.runs(), 0);
        assert_eq!(
            h.invokes.load(Ordering::SeqCst),
            1,
            "one attempt: no repair loop"
        );
    }
}

#[tokio::test]
async fn fx_never_executes_a_tool_and_never_sees_one() {
    // FX is handed only a provider port. The executor of tools is Chip's, and the only run
    // count that moves is the one Chip's executor owns.
    let h = harness(vec![
        Attempt::Reply(REQUEST_SELFTEST.into()),
        Attempt::Reply(COMPLETE.into()),
    ]);
    run(&h).await;
    assert_eq!(h.executor.runs(), 1);
    for request in h.requests.lock().unwrap().iter() {
        assert!(request.tools.is_empty());
        assert!(
            !request
                .messages
                .iter()
                .flat_map(|m| &m.parts)
                .any(|p| matches!(p, Part::ToolCall(_) | Part::ToolResult(_))),
            "no tool call or result crosses into FX"
        );
    }
}

#[tokio::test]
async fn a_tool_that_fails_is_an_observation_not_a_retry_of_inference() {
    let h = harness_with(
        SequencePort::new(vec![
            Attempt::Reply(REQUEST_SELFTEST.into()),
            Attempt::Reply(r#"{"decision":"block","reason":"the self test failed"}"#.into()),
        ]),
        CountingExecutor::failing(),
        OptionsPolicy::DropUnsupported,
        None,
    );
    let report = run(&h).await;
    assert!(
        matches!(report.outcome, WorkOutcome::Blocked { .. }),
        "{:?}",
        report.outcome
    );
    assert_eq!(h.executor.runs(), 1);
    assert_eq!(h.invokes.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------- failure and delivery

#[tokio::test]
async fn a_provider_failure_reaches_chip_with_its_delivery_and_is_not_retried() {
    let h = harness(vec![
        Attempt::Fail(failure(
            PortFailureKind::ProviderError,
            Observed::Invoked,
            Some(429),
        )),
        Attempt::Reply(COMPLETE.into()),
    ]);
    let report = run(&h).await;
    let text = reason(&report);
    assert!(matches!(report.outcome, WorkOutcome::Failed { .. }));
    assert!(text.contains("rate_limited"), "{text}");
    assert!(
        text.contains("possibly_sent"),
        "an error never lowers delivery: {text}"
    );
    assert!(text.contains("status 429"), "{text}");
    assert!(text.contains("not retried"), "{text}");
    assert_eq!(delivery(&report), Some(Delivery::PossiblySent));
    assert!(
        report.events.iter().any(|e| matches!(
            e,
            WorkEvent::WorkFailed {
                delivery: Some(Delivery::PossiblySent),
                ..
            }
        )),
        "the terminal event carries the same certainty"
    );
    assert_eq!(
        h.invokes.load(Ordering::SeqCst),
        1,
        "the ambiguous attempt was not repeated"
    );
    assert!(
        report.events.iter().any(|e| matches!(
            e,
            WorkEvent::ModelCalled {
                succeeded: false,
                ..
            }
        )),
        "the failed call is on the event stream"
    );
}

#[tokio::test]
async fn a_call_that_never_left_is_reported_definitely_unsent_as_a_typed_field() {
    let h = harness(vec![Attempt::Fail(failure(
        PortFailureKind::ProviderError,
        Observed::NotInvoked,
        None,
    ))]);
    let report = run(&h).await;
    assert_eq!(delivery(&report), Some(Delivery::DefinitelyUnsent));
}

#[tokio::test]
async fn a_failure_that_is_not_a_model_call_carries_no_delivery() {
    let h = harness(vec![Attempt::Reply("not json at all".into())]);
    let report = run(&h).await;
    assert!(reason(&report).contains("not a valid decision"));
    assert_eq!(delivery(&report), None);
}

#[tokio::test]
async fn an_ambiguous_delivery_does_not_cause_a_duplicate_provider_call() {
    for failure_kind in [
        PortFailureKind::ProviderError,
        PortFailureKind::Timeout,
        PortFailureKind::IncompleteStream,
    ] {
        let h = harness(vec![
            Attempt::Fail(failure(failure_kind, Observed::Invoked, None)),
            Attempt::Reply(COMPLETE.into()),
        ]);
        let report = run(&h).await;
        assert!(
            matches!(report.outcome, WorkOutcome::Failed { .. }),
            "{failure_kind:?}: {:?}",
            report.outcome
        );
        assert_eq!(
            h.invokes.load(Ordering::SeqCst),
            1,
            "{failure_kind:?}: exactly one provider call"
        );
        assert_eq!(h.executor.runs(), 0);
    }
}

#[tokio::test]
async fn a_stream_that_ends_without_a_verified_finish_is_not_a_success() {
    let h = harness(vec![
        Attempt::Cut(COMPLETE.into()),
        Attempt::Reply(COMPLETE.into()),
    ]);
    let report = run(&h).await;
    // The cut stream carried a perfectly valid decision, and it still must not be used.
    assert!(
        matches!(report.outcome, WorkOutcome::Failed { .. }),
        "{:?}",
        report.outcome
    );
    assert!(
        reason(&report).contains("delivery confirmed_response"),
        "{}",
        reason(&report)
    );
    assert_eq!(delivery(&report), Some(Delivery::ConfirmedResponse));
    assert_eq!(h.invokes.load(Ordering::SeqCst), 1);
    assert_eq!(h.executor.runs(), 0);
}

#[tokio::test]
async fn a_request_fx_refuses_never_reaches_the_provider() {
    // Under `Refuse` the sampling options go to FX, whose port here cannot carry them.
    let h = harness_with(
        SequencePort::new(vec![Attempt::Reply(COMPLETE.into())])
            .with_support(PortSupport::default()),
        CountingExecutor::new(),
        OptionsPolicy::Refuse,
        None,
    );
    let report = run(&h).await;
    assert!(
        matches!(report.outcome, WorkOutcome::Failed { .. }),
        "{:?}",
        report.outcome
    );
    // The first refusal is Chip's own (temperature cannot be carried): nothing reached FX.
    assert_eq!(
        h.invokes.load(Ordering::SeqCst),
        0,
        "refused before the provider"
    );
}

#[tokio::test]
async fn fx_refuses_unsupported_features_before_invocation_with_nothing_sent() {
    use chip_inference::{ModelProvider, ModelRequest};
    let provider = FxModelProvider::new(
        SequencePort::new(vec![Attempt::Reply("unused".into())]),
        RequestIds::new([1; 8]),
        OptionsPolicy::Refuse,
    );
    // No temperature, but a max_tokens the port cannot carry.
    let mut request = ModelRequest::new(
        "m",
        vec![chip_inference::Message::new(
            chip_inference::MessageRole::User,
            "hi",
        )],
    );
    request.temperature = None;
    let error = provider.complete(request).await.unwrap_err();
    let text = error.to_string();
    assert!(text.contains("capability_unavailable"), "{text}");
    assert!(
        text.contains("definitely_unsent"),
        "refusal before dispatch proves nothing was sent: {text}"
    );
    assert_eq!(provider.with_port(|p| p.invokes.load(Ordering::SeqCst)), 0);
}

// ---------------------------------------------------------------- cancellation

struct Flag(Arc<AtomicBool>);
impl CancelSignal for Flag {
    fn cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn cancellation_before_invocation_sends_nothing() {
    let flag = Arc::new(AtomicBool::new(true));
    let h = harness_with(
        SequencePort::new(vec![Attempt::Reply(COMPLETE.into())]),
        CountingExecutor::new(),
        OptionsPolicy::DropUnsupported,
        Some(Arc::new(Flag(flag))),
    );
    let report = run(&h).await;
    let text = reason(&report);
    assert!(text.contains("cancelled"), "{text}");
    assert!(text.contains("definitely_unsent"), "{text}");
    assert_eq!(
        h.invokes.load(Ordering::SeqCst),
        0,
        "the provider was never called"
    );
}

#[tokio::test]
async fn a_cancellation_scoped_to_one_call_interrupts_that_call_only() {
    // The provider has no signal of its own (it may be shared by many works); the caller's
    // signal is scoped to its call with `with_call_cancel`.
    let flag = Arc::new(AtomicBool::new(false));
    let h = harness_with(
        SequencePort::new(vec![
            Attempt::CancelWhileStreaming(COMPLETE.into()),
            Attempt::Reply(COMPLETE.into()),
        ])
        .with_cancel_flag(flag.clone()),
        CountingExecutor::new(),
        OptionsPolicy::DropUnsupported,
        None,
    );
    let report = with_call_cancel(Arc::new(Flag(flag.clone())), run(&h)).await;
    assert!(reason(&report).contains("cancelled"), "{}", reason(&report));
    assert_eq!(delivery(&report), Some(Delivery::ConfirmedResponse));
    assert_eq!(h.invokes.load(Ordering::SeqCst), 1);

    // Outside the scope the same flag means nothing to the provider: the next call completes.
    let report = run(&h).await;
    assert!(
        matches!(report.outcome, WorkOutcome::Completed { .. }),
        "{:?}",
        report.outcome
    );
}

#[tokio::test]
async fn cancellation_after_invocation_is_never_reported_as_unsent() {
    let flag = Arc::new(AtomicBool::new(false));
    let h = harness_with(
        SequencePort::new(vec![
            Attempt::CancelWhileStreaming(COMPLETE.into()),
            Attempt::Reply(COMPLETE.into()),
        ])
        .with_cancel_flag(flag.clone()),
        CountingExecutor::new(),
        OptionsPolicy::DropUnsupported,
        Some(Arc::new(Flag(flag))),
    );
    let report = run(&h).await;
    let text = reason(&report);
    assert!(
        matches!(report.outcome, WorkOutcome::Failed { .. }),
        "{:?}",
        report.outcome
    );
    assert!(text.contains("cancelled"), "{text}");
    assert!(
        !text.contains("definitely_unsent"),
        "the provider was invoked: {text}"
    );
    assert!(
        text.contains("confirmed_response"),
        "output had been observed: {text}"
    );
    assert_eq!(
        h.invokes.load(Ordering::SeqCst),
        1,
        "no second call after a cancel"
    );
}
