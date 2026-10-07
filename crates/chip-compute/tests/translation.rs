//! Offline tests: adapter construction, request/result/error translation.

use std::path::Path;
use std::time::Duration;

use chip_compute::{
    ComputeExecutor, ComputeOperation, SELFTEST_INTENT, build_arguments, translate_failure,
    translate_result,
};
use chip_core::{ExecutionError, ExecutionId, ExecutionRequest, ExecutionStatus, Executor};

fn id() -> ExecutionId {
    ExecutionId::new("exec-1")
}

#[test]
fn request_translation_uses_exec_with_runtime_and_json() {
    let op = ComputeOperation::python("print(1)");
    let args = build_arguments(
        &op,
        Path::new("/tmp/x/operation.py"),
        Duration::from_secs(30),
    );
    assert_eq!(
        args,
        [
            "exec",
            "/tmp/x/operation.py",
            "--runtime",
            "python",
            "--timeout",
            "30s",
            "--json"
        ]
    );
}

#[test]
fn success_result_preserves_output_and_receipt() {
    let json = r#"{"status":"completed","exit_code":0,"stdout":{"text":"hello\n"},"stderr":{"text":""},
        "receipt":{"receipt_hash":"sha256:abc","execution_id":"exec_9"},"error":null}"#;
    let result = translate_result(id(), json.as_bytes()).unwrap();
    assert_eq!(result.id, id());
    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(result.output, "hello");
    assert_eq!(result.receipt_id.as_deref(), Some("sha256:abc"));
}

#[test]
fn nonzero_exit_and_timeout_are_failure_results_with_receipt() {
    let nonzero = r#"{"status":"completed","exit_code":3,"stdout":{"text":""},"stderr":{"text":"bad\n"},
        "receipt":{"receipt_hash":"sha256:r1"},"error":null}"#;
    let result = translate_result(id(), nonzero.as_bytes()).unwrap();
    assert_eq!(result.status, ExecutionStatus::Failure);
    assert!(result.output.contains("bad"));
    assert_eq!(result.receipt_id.as_deref(), Some("sha256:r1"));

    let timeout = r#"{"status":"timed_out","exit_code":null,"stdout":{"text":""},"stderr":{"text":""},
        "receipt":{"receipt_hash":"sha256:r2"},"error":{"kind":"timeout","message":"wall time limit exceeded"}}"#;
    let result = translate_result(id(), timeout.as_bytes()).unwrap();
    assert_eq!(result.status, ExecutionStatus::Failure);
    assert!(result.output.contains("wall time limit exceeded"));
}

#[test]
fn cancelled_and_unreadable_results_map_to_errors() {
    let cancelled = r#"{"status":"cancelled","exit_code":null,"stdout":{"text":""}}"#;
    assert_eq!(
        translate_result(id(), cancelled.as_bytes()),
        Err(ExecutionError::Cancelled)
    );
    assert!(matches!(
        translate_result(id(), b"not json"),
        Err(ExecutionError::ExecutionFailed(_))
    ));
}

#[test]
fn compute_failure_envelopes_map_to_execution_errors() {
    let env = |code: &str| {
        format!("human text\n{{\"error\":{{\"code\":\"{code}\",\"message\":\"m\"}},\"ok\":false}}")
    };
    assert!(matches!(
        translate_failure(&env("unknown_runtime")),
        ExecutionError::ExecutorUnavailable(_)
    ));
    assert!(matches!(
        translate_failure(&env("controller_unavailable")),
        ExecutionError::ExecutorUnavailable(_)
    ));
    assert!(matches!(
        translate_failure(&env("invalid_workload")),
        ExecutionError::InvalidRequest(_)
    ));
    assert!(matches!(
        translate_failure(&env("io_error")),
        ExecutionError::ExecutionFailed(_)
    ));
    assert!(matches!(
        translate_failure("no envelope"),
        ExecutionError::ExecutionFailed(_)
    ));
}

#[tokio::test]
async fn adapter_constructs_and_rejects_unconfigured_intent() {
    let executor = ComputeExecutor::with_binary("compute-does-not-exist");
    let err = executor
        .execute(ExecutionRequest::new(id(), "rm -rf /"))
        .await
        .unwrap_err();
    assert!(matches!(err, ExecutionError::InvalidRequest(_)), "{err:?}");
}

#[tokio::test]
async fn missing_binary_is_executor_unavailable() {
    let executor = ComputeExecutor::with_binary("compute-does-not-exist");
    let err = executor
        .execute(ExecutionRequest::new(id(), SELFTEST_INTENT))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ExecutionError::ExecutorUnavailable(_)),
        "{err:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_execute_stops_waiting_for_the_client_process() {
    use std::os::unix::fs::PermissionsExt;
    // A stand-in executable that never finishes; proves we stop waiting.
    let dir = std::env::temp_dir().join(format!("chip-compute-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("fake-compute");
    std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let executor = ComputeExecutor::with_binary(&script);
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_millis(300),
        executor.execute(ExecutionRequest::new(id(), SELFTEST_INTENT)),
    )
    .await;
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(dir);
}
