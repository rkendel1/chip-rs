//! Live test: one real execution through the Compute CLI.
//!
//! Skips explicitly when Compute is unavailable (set COMPUTE_BIN, or put
//! `compute` on PATH). Run: cargo test -p chip-compute --test live_compute -- --nocapture

use std::sync::Arc;

use chip_compute::{
    ComputeExecutor, HASH_EXPECTED_SHA256, HASH_INTENT, OP_A_INTENT, OP_C_INTENT, SELFTEST_INTENT,
    SELFTEST_OUTPUT,
};
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

/// Requests the self test, then completes only if the observation says it completed.
struct SelfTestThenFinish;

impl chip_core::LocalWorkPolicy for SelfTestThenFinish {
    fn propose(&self, view: &chip_core::WorkView<'_>) -> Option<chip_core::WorkDecision> {
        use chip_core::{CapabilityId, CapabilityRequest, ObservationKind, WorkDecision};
        match view.observations.last() {
            None => Some(WorkDecision::RequestCapability(CapabilityRequest::new(
                ExecutionId::new("live-work-1"),
                CapabilityId::new(SELFTEST_INTENT).unwrap(),
            ))),
            Some(o) if o.kind == ObservationKind::ExecutionCompleted => {
                Some(WorkDecision::Complete {
                    summary: "the self test completed".into(),
                })
            }
            Some(_) => Some(WorkDecision::Block {
                reason: "the self test did not complete".into(),
            }),
        }
    }
}

struct NeverEscalates;

impl chip_core::WorkDecisionBoundary for NeverEscalates {
    fn interpret(
        &self,
        _r: &fx_core::ModelResponse,
        _c: &[chip_core::Capability],
    ) -> Result<chip_core::WorkDecision, chip_core::DecisionError> {
        Err(chip_core::DecisionError::InvalidDecision(
            "no escalation expected".into(),
        ))
    }
}

#[tokio::test]
async fn live_bounded_work_loop() {
    use chip_core::{
        CapabilityAvailability, ExecutionObserver, WorkGoal, WorkId, WorkOutcome, WorkSpec,
    };
    let compute = Arc::new(ComputeExecutor::new());
    let agent = Agent::new(Arc::new(NoModel))
        .with_capabilities(compute.clone())
        .with_executor(compute)
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(chip_core::TestLocalReasoner::default().on(
            chip_core::EvidenceState::Unknown,
            chip_core::LocalReasoningResult::Continue {
                rationale: "live".into(),
            },
        )));
    if let Ok(found) = agent.discover_capabilities().await.result {
        for capability in found {
            if let CapabilityAvailability::Unavailable(reason)
            | CapabilityAvailability::Misconfigured(reason) = capability.availability
            {
                eprintln!("SKIPPED — Compute unavailable ({reason})");
                return;
            }
        }
    }
    let spec = WorkSpec::new(WorkId::new("live-work"), WorkGoal::new("run the self test"));
    let report = agent
        .run_work(&spec, &SelfTestThenFinish, &NeverEscalates)
        .await;

    // One call: execution, receipt, observation and the next decision all happened inside it.
    assert_eq!(
        report.outcome,
        WorkOutcome::Completed {
            summary: "the self test completed".into()
        }
    );
    assert_eq!(
        (
            report.summary.turns,
            report.summary.executions,
            report.summary.model_escalations
        ),
        (2, 1, 0)
    );
    let receipt = report.observations[0]
        .receipt_id
        .clone()
        .expect("Compute returns a receipt");
    assert!(receipt.starts_with("sha256:"), "{receipt}");

    // The measurement of the real run: Compute latency is observed separately and is real.
    let m = report.measurement();
    assert_eq!((m.executions, m.observations, m.model_calls), (1, 1, 0));
    assert!(
        m.compute_latency > std::time::Duration::ZERO,
        "a real execution takes measurable time"
    );
    assert!(m.total_latency >= m.compute_latency);
    assert_eq!(m.model_latency, std::time::Duration::ZERO);
    assert!(chip_core::verify_trajectory(&report.events, &spec.limits).is_empty());
    eprintln!(
        "PASSED — real bounded work loop (receipt {receipt}; compute {:?} of {:?} total)",
        m.compute_latency, m.total_latency
    );
}

/// The digest is produced by real Compute and compared with a value known independently.
#[tokio::test]
async fn live_compute_hash_matches_the_known_digest() {
    let executor = ComputeExecutor::new().with_standard_operations();
    let request = ExecutionRequest::new(ExecutionId::new("live-hash"), HASH_INTENT);
    match executor.execute(request).await {
        Err(ExecutionError::ExecutorUnavailable(reason)) => {
            eprintln!("SKIPPED — Compute-configured unavailable ({reason})");
        }
        Err(other) => panic!("real Compute execution failed: {other}"),
        Ok(result) => {
            assert_eq!(result.status, ExecutionStatus::Success, "{}", result.output);
            assert_eq!(result.output, HASH_EXPECTED_SHA256);
            let receipt = result.receipt_id.expect("Compute should return a receipt");
            assert!(receipt.starts_with("sha256:"), "{receipt}");
            eprintln!("PASSED — real Compute hash (receipt {receipt})");
        }
    }
}

/// The opaque ids run the same real operations: op_a is the digest, op_c is the self test.
#[tokio::test]
async fn live_compute_opaque_operations_run_their_described_operation() {
    let executor = ComputeExecutor::new().with_opaque_operations();
    for (id, want) in [
        (OP_A_INTENT, HASH_EXPECTED_SHA256),
        (OP_C_INTENT, SELFTEST_OUTPUT),
    ] {
        let request = ExecutionRequest::new(ExecutionId::new("live-opaque"), id);
        match executor.execute(request).await {
            Err(ExecutionError::ExecutorUnavailable(reason)) => {
                eprintln!("SKIPPED — Compute-configured unavailable ({reason})");
                return;
            }
            Err(other) => panic!("real Compute execution failed: {other}"),
            Ok(result) => {
                assert_eq!(result.status, ExecutionStatus::Success, "{}", result.output);
                assert_eq!(result.output, want, "{id}");
                assert!(result.receipt_id.is_some_and(|r| r.starts_with("sha256:")));
            }
        }
    }
}

/// Every dealing of the ids runs the real operation its description promises: the id carrying the
/// digest produces the independently known digest, the one carrying the self test, the self test.
#[tokio::test]
async fn live_compute_every_assignment_runs_the_described_operation() {
    const IDS: [&str; 3] = ["compute.op_a", "compute.op_b", "compute.op_c"];
    for [h, s, t] in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let executor = ComputeExecutor::new().with_opaque_assignment(IDS[h], IDS[s], IDS[t]);
        for (id, want) in [(IDS[h], HASH_EXPECTED_SHA256), (IDS[t], SELFTEST_OUTPUT)] {
            let request = ExecutionRequest::new(ExecutionId::new("live-assign"), id);
            match executor.execute(request).await {
                Err(ExecutionError::ExecutorUnavailable(reason)) => {
                    eprintln!("SKIPPED — Compute-configured unavailable ({reason})");
                    return;
                }
                Err(other) => panic!("real Compute execution failed: {other}"),
                Ok(result) => {
                    assert_eq!(result.status, ExecutionStatus::Success, "{}", result.output);
                    assert_eq!(result.output, want, "{id} in assignment {h}{s}{t}");
                    assert!(result.receipt_id.is_some_and(|r| r.starts_with("sha256:")));
                }
            }
        }
    }
}
