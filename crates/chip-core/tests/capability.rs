//! PR5: capabilities are declared, discoverable and separate from execution.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityEvent,
    CapabilityId, CapabilityProvider, ExecutionError, ExecutionId, ExecutionRequest,
    ExecutionResult, Executor, TestExecutor,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse};

struct NoModel;

#[async_trait::async_trait]
impl ModelProvider for NoModel {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        panic!("capability discovery must not call the model");
    }
}

struct CountingExecutor(AtomicUsize);

#[async_trait::async_trait]
impl Executor for CountingExecutor {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ExecutionResult::success(r.id, "ran"))
    }
}

struct TestCapabilities {
    availability: CapabilityAvailability,
}

#[async_trait::async_trait]
impl CapabilityProvider for TestCapabilities {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(vec![CapabilityDescriptor::new(
            CapabilityId::new("test.echo").unwrap(),
            "Echo",
            "Deterministic test capability",
        )])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        self.availability.clone()
    }
}

fn agent(availability: CapabilityAvailability) -> Agent {
    Agent::new(Arc::new(NoModel)).with_capabilities(Arc::new(TestCapabilities { availability }))
}

#[test]
fn capability_ids_are_stable_opaque_names_not_commands() {
    let id = CapabilityId::new("compute.selftest").unwrap();
    assert_eq!(id.as_str(), "compute.selftest");
    assert_eq!(id.to_string(), "compute.selftest");
    for bad in ["", "rm -rf /", "a b", "UPPER", "x;y", "$(id)", "/bin/sh"] {
        assert_eq!(
            CapabilityId::new(bad),
            Err(CapabilityError::InvalidId(bad.to_string())),
            "{bad:?}"
        );
    }
}

#[tokio::test]
async fn discovery_returns_descriptors_and_semantic_events() {
    let report = agent(CapabilityAvailability::Available)
        .discover_capabilities()
        .await;
    let found = report.result.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].descriptor.id.as_str(), "test.echo");
    assert_eq!(found[0].descriptor.name, "Echo");
    assert_eq!(found[0].availability, CapabilityAvailability::Available);
    assert_eq!(
        report.events,
        vec![
            CapabilityEvent::CapabilitiesRequested,
            CapabilityEvent::CapabilitiesAvailable { count: 1 }
        ]
    );
}

#[tokio::test]
async fn discovery_never_invokes_the_executor() {
    let executor = Arc::new(CountingExecutor(AtomicUsize::new(0)));
    let agent = agent(CapabilityAvailability::Available).with_executor(executor.clone());
    agent.discover_capabilities().await.result.unwrap();
    assert_eq!(executor.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn capabilities_are_optional() {
    let report = Agent::new(Arc::new(NoModel)).discover_capabilities().await;
    assert!(matches!(
        report.result,
        Err(CapabilityError::Unavailable(_))
    ));
    assert!(matches!(
        report.events.last(),
        Some(CapabilityEvent::CapabilitiesUnavailable { .. })
    ));
}

#[tokio::test]
async fn known_capability_becomes_an_execution_request() {
    let agent = agent(CapabilityAvailability::Available).with_executor(Arc::new(TestExecutor));
    let id = CapabilityId::new("test.echo").unwrap();
    let request = agent
        .request_for_capability(ExecutionId::new("e1"), &id)
        .await
        .unwrap();
    assert_eq!(
        request,
        ExecutionRequest::new(ExecutionId::new("e1"), "test.echo")
    );
    let report = agent.execute(request).await;
    assert!(report.result.is_ok());
}

#[tokio::test]
async fn unknown_or_unusable_capability_never_becomes_a_request() {
    let unknown = CapabilityId::new("test.other").unwrap();
    let err = agent(CapabilityAvailability::Available)
        .request_for_capability(ExecutionId::new("e"), &unknown)
        .await
        .unwrap_err();
    assert_eq!(err, CapabilityError::Unknown("test.other".into()));

    for state in [
        CapabilityAvailability::Unavailable("gone".into()),
        CapabilityAvailability::Misconfigured("bad".into()),
    ] {
        let known = CapabilityId::new("test.echo").unwrap();
        let err = agent(state)
            .request_for_capability(ExecutionId::new("e"), &known)
            .await
            .unwrap_err();
        assert!(matches!(err, CapabilityError::Unavailable(_)));
    }

    let err = Agent::new(Arc::new(NoModel))
        .request_for_capability(ExecutionId::new("e"), &CapabilityId::new("a").unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, CapabilityError::Unavailable(_)));
}
