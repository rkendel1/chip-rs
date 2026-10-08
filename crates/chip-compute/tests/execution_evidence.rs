//! Compute execution identity travels with the execution's terminal record, from Compute's own
//! structured result only. Chip carries it; it never derives it.

use chip_compute::translate_result;
use std::collections::BTreeMap;

use chip_compute::COMPUTE_EVIDENCE_RUNTIME;
use chip_core::{ExecutionEvidence, ExecutionId, ExecutionObserver, ExecutionStatus, Observer};

const CHIP_ID: &str = "chip-exec-1";

fn translate(json: &str) -> chip_core::ExecutionResult {
    translate_result(ExecutionId::new(CHIP_ID), json.as_bytes()).unwrap()
}

/// What Compute reported, as `identifier -> value`.
fn compute(result: &chip_core::ExecutionResult) -> Option<BTreeMap<String, String>> {
    result
        .evidence
        .as_ref()
        .and_then(|e| e.runtime(COMPUTE_EVIDENCE_RUNTIME).cloned())
}

fn ids(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn carries_compute_execution_and_receipt_identity_exactly() {
    let result = translate(
        r#"{"execution_id":"exec_789","status":"completed","exit_code":0,
            "stdout":{"text":"ok\n"},"stderr":{"text":""},"receipt":{"receipt_hash":"sha256:abc"}}"#,
    );
    assert_eq!(result.status, ExecutionStatus::Success);
    // Compute's `exec` result names an execution and a receipt but no environment or job, so
    // exactly those two are reported and nothing else is invented.
    assert_eq!(
        compute(&result),
        Some(ids(&[
            ("executionId", "exec_789"),
            ("receiptId", "sha256:abc")
        ]))
    );
    // The pre-existing receipt field is unchanged.
    assert_eq!(result.receipt_id.as_deref(), Some("sha256:abc"));
}

#[test]
fn partial_compute_identity_is_not_completed_by_invention() {
    let result = translate(
        r#"{"execution_id":"exec_789","status":"completed","exit_code":0,"stdout":{"text":""},"stderr":{"text":""}}"#,
    );
    assert_eq!(compute(&result), Some(ids(&[("executionId", "exec_789")])));
}

#[test]
fn no_compute_identity_means_no_evidence() {
    let result = translate(
        r#"{"status":"completed","exit_code":0,"stdout":{"text":"hi"},"stderr":{"text":""}}"#,
    );
    assert!(result.evidence.is_none());
}

#[test]
fn failure_text_is_never_read_as_compute_identity() {
    // A real failure on this machine: the message names a command, not a Compute identifier.
    let result = translate(
        r#"{"status":"failed","exit_code":127,"stdout":{"text":""},
            "stderr":{"text":"sh: cargo: command not found"},
            "error":{"message":"sh: cargo: command not found"}}"#,
    );
    assert_eq!(result.status, ExecutionStatus::Failure);
    assert!(result.output.contains("sh: cargo: command not found"));
    assert!(result.evidence.is_none());
}

#[test]
fn failure_with_compute_identity_keeps_the_failure_and_adds_the_evidence() {
    let result = translate(
        r#"{"execution_id":"exec_9","status":"failed","exit_code":127,"stdout":{"text":""},
            "stderr":{"text":"sh: cargo: command not found"},
            "receipt":{"receipt_hash":"sha256:def"}}"#,
    );
    assert_eq!(result.status, ExecutionStatus::Failure);
    assert!(result.output.contains("sh: cargo: command not found"));
    assert_eq!(
        compute(&result),
        Some(ids(&[
            ("executionId", "exec_9"),
            ("receiptId", "sha256:def")
        ]))
    );
}

#[test]
fn chip_identity_stays_distinct_from_compute_identity() {
    let result = translate(
        r#"{"execution_id":"exec_789","status":"completed","exit_code":0,"stdout":{"text":""},"stderr":{"text":""}}"#,
    );
    assert_eq!(result.id.to_string(), CHIP_ID);
    assert_ne!(compute(&result).unwrap()["executionId"], CHIP_ID);
}

#[test]
fn blank_identifiers_are_dropped_and_empty_evidence_is_omitted() {
    let result = translate(
        r#"{"execution_id":"  ","status":"completed","exit_code":0,"stdout":{"text":""},"stderr":{"text":""}}"#,
    );
    assert!(result.evidence.is_none());
}

#[test]
fn the_terminal_observation_carries_the_evidence_but_the_model_never_sees_it() {
    let result = translate(
        r#"{"execution_id":"exec_789","status":"completed","exit_code":0,
            "stdout":{"text":"ok"},"stderr":{"text":""},"receipt":{"receipt_hash":"sha256:abc"}}"#,
    );
    let observation = ExecutionObserver.observe(&result).unwrap();
    assert_eq!(observation.evidence, result.evidence);
    let rendered = observation.render();
    assert!(!rendered.contains("exec_789"), "{rendered}");
    // Only the pre-existing receipt line is rendered, exactly as before.
    assert!(rendered.contains("receipt_id: \"sha256:abc\""));
}

#[test]
fn evidence_attached_by_hand_is_normalized() {
    let result = chip_core::ExecutionResult::success(ExecutionId::new("x"), "ok")
        .with_execution_evidence(
            ExecutionEvidence::from_runtime("compute", [("jobId", "")]).unwrap_or_default(),
        );
    assert!(result.evidence.is_none());
}
