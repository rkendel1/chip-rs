//! The wire format carries Chip's own types across the process boundary unchanged.

use std::collections::BTreeMap;

use chip_core::{
    CapabilityAvailability, CapabilityError, CapabilityId, ExecutionError, ExecutionId,
    ExecutionResult, ExecutionStatus, InputValue,
};
use chip_remote_env::{Request, Response};

fn inputs() -> BTreeMap<String, InputValue> {
    [
        (
            "path".to_string(),
            InputValue::Text("src/\"lib\".rs\n".into()),
        ),
        ("count".to_string(), InputValue::Integer(-3)),
        ("flag".to_string(), InputValue::Bool(true)),
    ]
    .into()
}

#[test]
fn requests_round_trip() {
    for request in [
        Request::Info,
        Request::Capabilities,
        Request::Availability {
            capability: "pax.test".into(),
        },
        Request::Validate {
            capability: "project.read".into(),
            inputs: inputs(),
        },
        Request::Execute {
            execution_id: "model-1".into(),
            capability: "project.write".into(),
            inputs: inputs(),
        },
    ] {
        assert_eq!(Request::decode(&request.encode()).unwrap(), request);
    }
}

#[test]
fn responses_round_trip() {
    let id = ExecutionId::new("model-1");
    for response in [
        Response::Availability(CapabilityAvailability::Available),
        Response::Availability(CapabilityAvailability::Unavailable("no".into())),
        Response::Validated(Ok(())),
        Response::Validated(Err(CapabilityError::InvalidInput("bad path".into()))),
        Response::Executed(Ok(ExecutionResult {
            id: id.clone(),
            status: ExecutionStatus::Failure,
            output: "line\n\"quoted\"".into(),
            receipt_id: Some("sha256:abc".into()),
        })),
        Response::Executed(Ok(ExecutionResult::success(id, "ok"))),
        Response::Executed(Err(ExecutionError::ExecutionFailed("boom".into()))),
        Response::Executed(Err(ExecutionError::Cancelled)),
    ] {
        assert_eq!(Response::decode(&response.encode()).unwrap(), response);
    }
    let _ = CapabilityId::new("pax.test").unwrap();
}

#[test]
fn malformed_or_unversioned_messages_are_refused_not_repaired() {
    for bad in [
        "",
        "not json",
        "{}",
        r#"{"v":2,"op":"info"}"#,
        r#"{"v":1,"op":"nope"}"#,
        r#"{"v":1,"op":"execute","capability":"x"}"#,
        r#"{"v":1,"op":"validate","capability":"x","inputs":{"a":{"text":1}}}"#,
        r#"{"v":1,"op":"validate","capability":"x","inputs":{"a":{"text":"s","extra":1}}}"#,
    ] {
        assert!(Request::decode(bad).is_err(), "{bad:?}");
    }
    for bad in [
        "",
        "garbage",
        r#"{"v":1,"op":"execute"}"#,
        r#"{"v":1,"op":"execute","result":{"status":"win"}}"#,
    ] {
        assert!(Response::decode(bad).is_err(), "{bad:?}");
    }
}
