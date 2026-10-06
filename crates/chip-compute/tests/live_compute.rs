//! Live test: one real execution through the Compute CLI.
//!
//! Skips explicitly when Compute is unavailable (set COMPUTE_BIN, or put
//! `compute` on PATH). Run: cargo test -p chip-compute --test live_compute -- --nocapture

use std::sync::Arc;

use chip_compute::{ComputeExecutor, SELFTEST_INTENT, SELFTEST_OUTPUT};
use chip_core::{
    Agent, ExecutionError, ExecutionId, ExecutionRequest, ExecutionStatus, Executor, TestExecutor,
};

#[tokio::test]
async fn live_compute_execution() {
    let _ = TestExecutor; // never used as a substitute for Compute
    let executor = ComputeExecutor::new();
    let request = ExecutionRequest::new(ExecutionId::new("live-1"), SELFTEST_INTENT);

    match executor.execute(request.clone()).await {
        Err(ExecutionError::ExecutorUnavailable(reason)) => {
            eprintln!("SKIPPED — Compute-configured unavailable ({reason})");
        }
        Err(other) => panic!("real Compute execution failed: {other}"),
        Ok(result) => {
            assert_eq!(result.status, ExecutionStatus::Success, "{}", result.output);
            assert_eq!(result.output, SELFTEST_OUTPUT);
            let receipt = result.receipt_id.expect("Compute should return a receipt");
            assert!(receipt.starts_with("sha256:"), "{receipt}");

            // The same path through a Chip agent.
            let agent =
                Agent::new(Arc::new(NoModel)).with_executor(Arc::new(ComputeExecutor::new()));
            let report = agent.execute(request).await;
            assert_eq!(report.result.unwrap().output, SELFTEST_OUTPUT);
            eprintln!("PASSED — real Compute execution (receipt {receipt})");
        }
    }
}

struct NoModel;

#[async_trait::async_trait]
impl fx_core::ModelProvider for NoModel {
    async fn complete(
        &self,
        _r: fx_core::ModelRequest,
    ) -> Result<fx_core::ModelResponse, fx_core::FxError> {
        Err(fx_core::FxError::Provider("not used".into()))
    }
}
