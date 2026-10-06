//! PR5: the Compute adapter exposes its operations as capabilities.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use chip_compute::{ComputeExecutor, ComputeOperation, SELFTEST_INTENT};
use chip_core::{
    Agent, CapabilityAvailability, CapabilityError, CapabilityId, CapabilityProvider,
    ExecutionEvent, ExecutionId, ExecutionStatus,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse};

struct NoModel;

#[async_trait::async_trait]
impl ModelProvider for NoModel {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        panic!("model must not be called");
    }
}

/// A stand-in `compute` that records every invocation in `marker` and prints a
/// canned result with a receipt.
fn fake_compute(tag: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("chip-compute-caps-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("invoked");
    let script = dir.join("fake-compute");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ntouch '{}'\ncat <<'EOF'\n{{\"status\":\"completed\",\"exit_code\":0,\"stdout\":{{\"text\":\"chip-compute selftest ok\\n\"}},\"stderr\":{{\"text\":\"\"}},\"receipt\":{{\"receipt_hash\":\"sha256:fake\"}}}}\nEOF\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, marker)
}

#[tokio::test]
async fn exposes_configured_operations_without_executing() {
    let (script, marker) = fake_compute("discover");
    let executor = ComputeExecutor::with_binary(&script);

    let descriptors = executor.capabilities().await.unwrap();
    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].id.as_str(), "compute.selftest");
    assert_eq!(descriptors[0].name, "Compute Self Test");
    assert_eq!(
        descriptors[0].description,
        "Deterministic Compute execution test"
    );

    // Implementation details are not part of the descriptor.
    let shown = format!("{descriptors:?}");
    for detail in ["python", "print(", ".py", "fake-compute"] {
        assert!(!shown.contains(detail), "descriptor leaks {detail}");
    }

    let id = &descriptors[0].id;
    assert_eq!(
        executor.availability(id).await,
        CapabilityAvailability::Available
    );
    assert!(!marker.exists(), "discovery must not invoke Compute");
}

#[tokio::test]
async fn availability_is_semantic() {
    let missing = ComputeExecutor::with_binary("/definitely/not/here/compute");
    let id = CapabilityId::new(SELFTEST_INTENT).unwrap();
    assert!(matches!(
        missing.availability(&id).await,
        CapabilityAvailability::Unavailable(reason) if reason == "Compute is not installed"
    ));

    let (script, _) = fake_compute("misconfigured");
    let broken = ComputeExecutor::with_binary(&script)
        .with_operation("test.broken", ComputeOperation::python("  "))
        .unwrap();
    let broken_id = CapabilityId::new("test.broken").unwrap();
    assert!(matches!(
        broken.availability(&broken_id).await,
        CapabilityAvailability::Misconfigured(_)
    ));
    let unknown = CapabilityId::new("test.nothing").unwrap();
    assert!(matches!(
        broken.availability(&unknown).await,
        CapabilityAvailability::Unavailable(_)
    ));
}

#[test]
fn operations_must_be_registered_under_valid_capability_ids() {
    let err = ComputeExecutor::with_binary("compute")
        .with_operation("rm -rf /", ComputeOperation::python("print(1)"))
        .unwrap_err();
    assert!(matches!(err, CapabilityError::InvalidId(_)));
}

#[tokio::test]
async fn selected_capability_executes_through_compute_with_receipt() {
    let (script, marker) = fake_compute("execute");
    let executor = Arc::new(ComputeExecutor::with_binary(&script));
    let agent = Agent::new(Arc::new(NoModel))
        .with_capabilities(executor.clone())
        .with_executor(executor);

    // 1. discover (explicit)  2. select  3. request  4. execute
    let found = agent.discover_capabilities().await.result.unwrap();
    assert_eq!(found[0].availability, CapabilityAvailability::Available);
    assert!(!marker.exists());

    let selected = found[0].descriptor.id.clone();
    let request = agent
        .request_for_capability(ExecutionId::new("cap-1"), &selected)
        .await
        .unwrap();
    let report = agent.execute(request).await;

    let result = report.result.unwrap();
    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(result.output, "chip-compute selftest ok");
    assert_eq!(result.receipt_id.as_deref(), Some("sha256:fake"));
    assert!(matches!(
        report.events.last(),
        Some(ExecutionEvent::ExecutionCompleted { .. })
    ));
    assert!(marker.exists(), "execution should have invoked Compute");
}

#[tokio::test]
async fn unknown_capability_fails_before_execution() {
    let (script, marker) = fake_compute("unknown");
    let executor = Arc::new(ComputeExecutor::with_binary(&script));
    let agent = Agent::new(Arc::new(NoModel))
        .with_capabilities(executor.clone())
        .with_executor(executor);
    let err = agent
        .request_for_capability(
            ExecutionId::new("x"),
            &CapabilityId::new("compute.nope").unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(err, CapabilityError::Unknown("compute.nope".into()));
    assert!(!marker.exists());
}

#[tokio::test]
async fn run_turn_executes_a_capability_through_compute_with_receipt() {
    use chip_core::{AgentDecision, DecisionInput, ScriptedDecision, Turn};
    struct Model;
    #[async_trait::async_trait]
    impl ModelProvider for Model {
        async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
            Ok(ModelResponse::new("r", "ok", fx_core::Usage::new(1, 1)))
        }
    }
    let (script, marker) = fake_compute("run-turn");
    let executor = Arc::new(ComputeExecutor::with_binary(&script));
    let agent = Agent::new(Arc::new(Model))
        .with_decision_boundary(Arc::new(ScriptedDecision::new(
            DecisionInput::RequestCapability {
                execution_id: ExecutionId::new("turn-1"),
                capability_id: SELFTEST_INTENT.into(),
                inputs: Default::default(),
            },
        )))
        .with_capabilities(executor.clone())
        .with_executor(executor);

    let outcome = agent.run_turn(Turn::new("hi")).await.unwrap();
    assert!(matches!(
        outcome.decision,
        AgentDecision::RequestCapability(_)
    ));
    let result = outcome.execution.unwrap();
    assert_eq!(result.id, ExecutionId::new("turn-1"));
    assert_eq!(result.receipt_id.as_deref(), Some("sha256:fake"));
    assert!(marker.exists());
}

#[tokio::test]
async fn a_compute_result_passes_through_the_generic_observer() {
    use chip_core::{ExecutionObserver, ObservationKind, Observer};
    let (script, _) = fake_compute("observe");
    let executor = ComputeExecutor::with_binary(&script);
    let request = chip_core::ExecutionRequest::new(ExecutionId::new("o1"), SELFTEST_INTENT);
    let result = chip_core::Executor::execute(&executor, request)
        .await
        .unwrap();
    let observation = ExecutionObserver.observe(&result).unwrap();
    assert_eq!(observation.kind, ObservationKind::ExecutionCompleted);
    assert_eq!(observation.execution_id, ExecutionId::new("o1"));
    assert_eq!(
        observation.output.as_deref(),
        Some("chip-compute selftest ok")
    );
    assert_eq!(observation.receipt_id.as_deref(), Some("sha256:fake"));
}

#[tokio::test]
async fn a_compute_observation_informs_the_next_deterministic_turn() {
    use chip_core::{ExecutionObserver, Observer, Turn};
    use std::sync::Mutex;
    #[derive(Default)]
    struct Recording(Mutex<Vec<ModelRequest>>);
    #[async_trait::async_trait]
    impl ModelProvider for Recording {
        async fn complete(&self, r: ModelRequest) -> Result<ModelResponse, FxError> {
            self.0.lock().unwrap().push(r);
            Ok(ModelResponse::new("r", "ok", fx_core::Usage::new(1, 1)))
        }
    }
    let (script, _) = fake_compute("next-turn");
    let result = chip_core::Executor::execute(
        &ComputeExecutor::with_binary(&script),
        chip_core::ExecutionRequest::new(ExecutionId::new("n1"), SELFTEST_INTENT),
    )
    .await
    .unwrap();
    let observation = ExecutionObserver.observe(&result).unwrap();
    let model = Arc::new(Recording::default());
    Agent::new(model.clone())
        .turn_with_observations(Turn::new("next"), &[observation])
        .await
        .unwrap();
    let requests = model.0.lock().unwrap();
    assert!(requests[0].messages[0].content.contains("sha256:fake"));
    assert!(requests[0].messages[0].content.contains("n1"));
}
