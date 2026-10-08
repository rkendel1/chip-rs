//! PR8: an observation represents an execution result; it invents nothing.

use chip_core::{
    ExecutionId, ExecutionObserver, ExecutionResult, ExecutionStatus, Observation,
    ObservationError, ObservationKind, Observer,
};

fn result(status: ExecutionStatus, output: &str, receipt: Option<&str>) -> ExecutionResult {
    ExecutionResult {
        id: ExecutionId::new("exec-x"),
        status,
        output: output.to_string(),
        receipt_id: receipt.map(str::to_string),
        evidence: None,
    }
}

fn observe(r: &ExecutionResult) -> Observation {
    ExecutionObserver.observe(r).unwrap()
}

#[test]
fn successful_execution_is_completed_with_everything_preserved() {
    let o = observe(&result(ExecutionStatus::Success, "hello", Some("abc123")));
    assert_eq!(o.kind, ObservationKind::ExecutionCompleted);
    assert_eq!(o.status, ExecutionStatus::Success);
    assert_eq!(o.execution_id, ExecutionId::new("exec-x"));
    assert_eq!(o.output.as_deref(), Some("hello"));
    assert_eq!(o.receipt_id.as_deref(), Some("abc123"));
}

#[test]
fn failed_execution_is_an_observation_not_an_error() {
    let o = ExecutionObserver
        .observe(&result(ExecutionStatus::Failure, "boom", Some("r1")))
        .expect("a failed execution is valid reality");
    assert_eq!(o.kind, ObservationKind::ExecutionFailed);
    assert_eq!(o.status, ExecutionStatus::Failure);
    assert_eq!(o.output.as_deref(), Some("boom"));
    assert_eq!(o.receipt_id.as_deref(), Some("r1"));
}

#[test]
fn cancelled_execution_is_observed_as_cancelled() {
    let o = observe(&result(ExecutionStatus::Cancelled, "", None));
    assert_eq!(o.kind, ObservationKind::ExecutionCancelled);
    assert_eq!(o.status, ExecutionStatus::Cancelled);
    assert_eq!(o.output, None);
    assert_eq!(o.receipt_id, None);
}

#[test]
fn observation_is_deterministic() {
    let r = result(ExecutionStatus::Success, "same", Some("r"));
    assert_eq!(observe(&r), observe(&r));
    assert_eq!(observe(&r), observe(&r.clone()));
}

#[test]
fn identity_and_receipt_are_preserved_exactly() {
    for status in [
        ExecutionStatus::Success,
        ExecutionStatus::Failure,
        ExecutionStatus::Cancelled,
    ] {
        let r = result(status, "o", Some("sha256:deadbeef"));
        let o = observe(&r);
        assert_eq!(o.execution_id, r.id);
        assert_eq!(o.receipt_id, r.receipt_id);
        assert_eq!(o.status, r.status);
    }
}

#[test]
fn domain_like_output_is_not_interpreted() {
    for text in [
        "tests passed",
        "deployment complete",
        "build succeeded",
        "file created",
    ] {
        let o = observe(&result(ExecutionStatus::Success, text, None));
        assert_eq!(o.kind, ObservationKind::ExecutionCompleted);
        assert_eq!(o.output.as_deref(), Some(text));
    }
    // Output that claims success does not override a reported failure.
    let o = observe(&result(ExecutionStatus::Failure, "all tests passed", None));
    assert_eq!(o.kind, ObservationKind::ExecutionFailed);
}

#[test]
fn only_generic_kinds_exist() {
    for (kind, name) in [
        (ObservationKind::ExecutionCompleted, "execution.completed"),
        (ObservationKind::ExecutionFailed, "execution.failed"),
        (ObservationKind::ExecutionCancelled, "execution.cancelled"),
    ] {
        assert_eq!(kind.as_str(), name);
    }
}

#[test]
fn a_result_without_identity_cannot_be_observed() {
    let mut r = result(ExecutionStatus::Success, "x", None);
    r.id = ExecutionId::new("  ");
    assert!(matches!(
        ExecutionObserver.observe(&r),
        Err(ObservationError::InvalidResult(_))
    ));
}
