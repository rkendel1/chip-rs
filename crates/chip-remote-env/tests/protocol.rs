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
            evidence: None,
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

#[test]
fn execution_evidence_is_additive_on_the_wire() {
    use chip_core::ExecutionEvidence;
    let id = ExecutionId::new("model-1");
    // Without evidence the message has no `execution_evidence` key at all, as before.
    let plain = Response::Executed(Ok(ExecutionResult::success(id.clone(), "ok"))).encode();
    assert!(!plain.contains("execution_evidence"), "{plain}");
    // With evidence it round-trips, carrying only the identifiers present.
    let evidence = ExecutionEvidence::from_runtime(
        "rt",
        [("executionId", "exec_789"), ("receiptId", "sha256:abc")],
    )
    .unwrap();
    let with = Response::Executed(Ok(
        ExecutionResult::success(id, "ok").with_execution_evidence(evidence)
    ));
    let encoded = with.encode();
    assert!(encoded.contains(r#""executionId":"exec_789""#), "{encoded}");
    assert!(
        !encoded.contains("jobId") && !encoded.contains("environmentId"),
        "{encoded}"
    );
    assert_eq!(Response::decode(&encoded).unwrap(), with);
}

#[test]
fn a_peer_that_sends_no_or_empty_evidence_decodes_to_none() {
    let base = r#"{"v":1,"op":"execute","result":{"execution_id":"m","status":"success","output":"ok","receipt_id":null"#;
    for tail in [
        "}}",
        r#","execution_evidence":{"rt":{}}}}"#,
        r#","execution_evidence":{"rt":{"jobId":""}}}}"#,
        r#","execution_evidence":{"rt":{"bad key":"x"}}}}"#,
        r#","execution_evidence":"nope"}}"#,
    ] {
        let Response::Executed(Ok(result)) = Response::decode(&format!("{base}{tail}")).unwrap()
        else {
            panic!("not an executed result");
        };
        assert!(result.evidence.is_none(), "{tail}");
    }
}
