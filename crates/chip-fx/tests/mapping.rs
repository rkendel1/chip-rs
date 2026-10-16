//! The mapping between Chip's model port and the authoritative FX contract.

mod common;

use chip_fx::mapping::{OptionsPolicy, RequestIds, from_outcome, kind_label, to_fx_request};
use chip_fx::report::from_report_json;
use chip_inference::{Delivery, FxError, Message, MessageRole, ModelRequest};
use common::{Attempt, SequencePort};
use fx_core::{
    Cancellation, Completion, DeliveryCertainty, Disposition, ErrorKind, Failure, FailureOrigin,
    FinishReason, Limits, Metadata, NoHost, Outcome, PartialOutput, RemoteCancellation, RequestId,
    Usage, encode_report, execute,
};
use fx_single_attempt::SingleAttemptCapability;

fn request() -> ModelRequest {
    ModelRequest::new(
        "model-x",
        vec![
            Message::new(MessageRole::System, "observation"),
            Message::new(MessageRole::User, "goal"),
        ],
    )
}

fn completion(content: &str) -> Completion {
    Completion {
        content: content.to_owned(),
        content_truncated: false,
        reasoning_bytes: 0,
        proposals: Vec::new(),
        provider_records: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(Usage {
            input_tokens: Some(7),
            output_tokens: Some(3),
            ..Usage::default()
        }),
        metadata: Metadata::default(),
    }
}

fn failure(kind: ErrorKind, delivery: DeliveryCertainty) -> Failure {
    Failure {
        origin: FailureOrigin::Capability,
        kind,
        delivery,
        retry_after: Some(std::time::Duration::from_millis(3000)),
        status_hint: Some(429),
        detail: None,
        partial: Some(PartialOutput {
            content: "part".into(),
            content_truncated: false,
        }),
    }
}

#[test]
fn request_ids_are_unique_per_attempt_and_carry_the_hosts_seed() {
    let mut ids = RequestIds::new(*b"seedseed");
    let (a, b) = (ids.issue(), ids.issue());
    assert_ne!(a, b);
    assert_eq!(&a.0[..8], b"seedseed");
    assert_eq!(a.0[8..], 0u64.to_be_bytes());
    assert_eq!(b.0[8..], 1u64.to_be_bytes());
}

#[test]
fn a_chip_request_becomes_a_text_only_fx_request_with_nothing_added() {
    let fx = to_fx_request(
        &request(),
        RequestId([9; 16]),
        OptionsPolicy::DropUnsupported,
        None,
    )
    .unwrap();
    assert_eq!(fx.model.0, "model-x");
    assert_eq!(fx.messages.len(), 2);
    assert!(fx.tools.is_empty());
    assert!(fx.options.max_output_tokens.is_none());
    assert!(fx.budget.is_none());
    // FX, not this adapter, decides whether the request is valid.
    fx_core::validate_request(&fx, &Limits::default()).expect("a plain text request validates");
}

#[test]
fn refuse_policy_fails_closed_on_options_the_path_cannot_carry() {
    let error =
        to_fx_request(&request(), RequestId([9; 16]), OptionsPolicy::Refuse, None).unwrap_err();
    assert!(matches!(error, FxError::InvalidRequest(_)));
    let mut no_temperature = request();
    no_temperature.temperature = None;
    let fx = to_fx_request(
        &no_temperature,
        RequestId([9; 16]),
        OptionsPolicy::Refuse,
        None,
    )
    .unwrap();
    assert_eq!(
        fx.options.max_output_tokens,
        Some(256),
        "max_tokens goes to FX, which decides"
    );
}

#[test]
fn a_verified_completion_is_the_only_success() {
    let id = RequestId([1; 16]);
    let ok = from_outcome(
        &Outcome::Completed(completion("hello")),
        Some(Disposition::Complete),
        id,
    )
    .unwrap();
    assert_eq!(ok.output, "hello");
    assert_eq!((ok.usage.prompt_tokens, ok.usage.completion_tokens), (7, 3));
    for disposition in [
        Some(Disposition::LengthLimited),
        Some(Disposition::ToolProposals),
        Some(Disposition::ProviderFailure),
        Some(Disposition::InvalidCompletion),
        None,
    ] {
        let error =
            from_outcome(&Outcome::Completed(completion("x")), disposition, id).unwrap_err();
        let FxError::Inference(detail) = error else {
            panic!("{disposition:?}")
        };
        assert_eq!(
            detail.delivery,
            Delivery::ConfirmedResponse,
            "{disposition:?}"
        );
    }
    let mut truncated = completion("x");
    truncated.content_truncated = true;
    assert!(
        from_outcome(
            &Outcome::Completed(truncated),
            Some(Disposition::Complete),
            id
        )
        .is_err()
    );
}

#[test]
fn every_failure_keeps_its_kind_delivery_status_and_partial_output() {
    let id = RequestId([1; 16]);
    for kind in [
        ErrorKind::InvalidRequest,
        ErrorKind::CapabilityUnavailable,
        ErrorKind::Unauthenticated,
        ErrorKind::Forbidden,
        ErrorKind::ModelUnavailable,
        ErrorKind::RateLimited,
        ErrorKind::ProviderUnavailable,
        ErrorKind::Transport,
        ErrorKind::SystemResumed,
        ErrorKind::Timeout,
        ErrorKind::Cancelled,
        ErrorKind::ProtocolViolation,
        ErrorKind::ResourceLimit,
        ErrorKind::Internal,
    ] {
        for (certainty, expected) in [
            (
                DeliveryCertainty::DefinitelyUnsent,
                Delivery::DefinitelyUnsent,
            ),
            (DeliveryCertainty::PossiblySent, Delivery::PossiblySent),
            (
                DeliveryCertainty::ConfirmedResponse,
                Delivery::ConfirmedResponse,
            ),
        ] {
            let error =
                from_outcome(&Outcome::Failed(failure(kind, certainty)), None, id).unwrap_err();
            let FxError::Inference(detail) = error else {
                panic!()
            };
            assert_eq!(detail.kind, kind_label(kind));
            assert_eq!(
                detail.delivery, expected,
                "delivery is carried through, never lowered"
            );
            assert_eq!(detail.status, Some(429));
            assert_eq!(detail.retry_after_ms, Some(3000));
            assert_eq!(detail.partial_output.as_deref(), Some("part"));
        }
    }
}

#[test]
fn a_cancellation_is_a_failure_and_never_claims_the_request_was_unsent_unless_fx_did() {
    let id = RequestId([1; 16]);
    for (certainty, expected) in [
        (
            DeliveryCertainty::DefinitelyUnsent,
            Delivery::DefinitelyUnsent,
        ),
        (DeliveryCertainty::PossiblySent, Delivery::PossiblySent),
    ] {
        let outcome = Outcome::Cancelled(Cancellation {
            delivery: certainty,
            remote: RemoteCancellation::Unconfirmed,
            partial: None,
        });
        let FxError::Inference(detail) = from_outcome(&outcome, None, id).unwrap_err() else {
            panic!()
        };
        assert_eq!(detail.kind, "cancelled");
        assert_eq!(detail.delivery, expected);
    }
}

fn real_report(attempt: Attempt) -> (fx_core::ExecutionReport, RequestId) {
    let id = RequestId([5; 16]);
    let fx = to_fx_request(&request(), id, OptionsPolicy::DropUnsupported, None).unwrap();
    let mut capability = SingleAttemptCapability::new(SequencePort::new(vec![attempt]));
    (
        execute(&mut capability, &fx, &Limits::default(), &mut NoHost),
        id,
    )
}

#[test]
fn the_json_report_a_host_returns_means_exactly_what_the_typed_report_means() {
    for attempt in [
        Attempt::Reply("a reply".into()),
        Attempt::Cut("cut".into()),
        Attempt::Fail(common::failure(
            fx_single_attempt::PortFailureKind::ProviderError,
            fx_single_attempt::Observed::Invoked,
            Some(503),
        )),
    ] {
        let (report, id) = real_report(attempt.clone());
        let typed = from_outcome(&report.outcome, report.disposition, id);
        let json = String::from_utf8(encode_report(&report)).unwrap();
        let from_json = from_report_json(&json, id);
        assert_eq!(typed, from_json, "{attempt:?}");
    }
}

#[test]
fn a_report_that_is_not_exactly_the_documented_shape_is_refused() {
    let id = RequestId([1; 16]);
    let (report, _) = real_report(Attempt::Reply("ok".into()));
    let good = String::from_utf8(encode_report(&report)).unwrap();
    assert!(from_report_json(&good, id).is_ok());
    for bad in [
        "".to_owned(),
        "not json".to_owned(),
        good.replace("\"v\":1", "\"v\":2"),
        good.replace("\"disposition\":\"complete\"", "\"disposition\":\"fine\""),
        good.replace("\"disposition\":\"complete\"", "\"disposition\":7"),
        r#"{"v":1}"#.to_owned(),
        r#"{"v":1,"report":{}}"#.to_owned(),
        r#"{"v":1,"report":{"outcome":{"type":"nonsense"}}}"#.to_owned(),
    ] {
        assert!(
            matches!(from_report_json(&bad, id), Err(FxError::InvalidResponse(_))),
            "{bad}"
        );
    }
}

#[test]
fn carry_policy_sends_the_token_cap_and_the_temperature_in_thousandths() {
    for (temperature, milli) in [
        (0.0f32, 0u16),
        (0.7, 700),
        (1.0, 1000),
        (2.0, 2000),
        (0.0004, 0),
    ] {
        let mut r = request();
        r.max_tokens = Some(256);
        r.temperature = Some(temperature);
        let fx = to_fx_request(&r, RequestId([9; 16]), OptionsPolicy::Carry, None).unwrap();
        assert_eq!(fx.options.max_output_tokens, Some(256));
        assert_eq!(fx.options.temperature_milli, Some(milli), "{temperature}");
        fx_core::validate_request(&fx, &Limits::default()).expect("a carried request validates");
    }
    let mut r = request();
    r.max_tokens = None;
    r.temperature = None;
    let fx = to_fx_request(&r, RequestId([9; 16]), OptionsPolicy::Carry, None).unwrap();
    assert_eq!(
        (fx.options.max_output_tokens, fx.options.temperature_milli),
        (None, None)
    );
}

#[test]
fn carry_policy_rejects_a_temperature_outside_the_range_or_not_a_number() {
    for bad in [-0.1f32, 2.1, f32::NAN, f32::INFINITY] {
        let mut r = request();
        r.temperature = Some(bad);
        let error = to_fx_request(&r, RequestId([9; 16]), OptionsPolicy::Carry, None).unwrap_err();
        assert!(matches!(error, FxError::InvalidRequest(_)), "{bad}");
    }
}
