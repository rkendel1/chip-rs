//! PR11: local evidence reuse. Only observations of real executions become
//! evidence; a repeated operation is answered locally with no model or execution.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, AgentError, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityInput, CapabilityProvider, CapabilityRequest, EvidenceError, EvidenceKey,
    EvidenceLookup, EvidenceOutcome, ExecutionError, ExecutionId, ExecutionObserver,
    ExecutionRequest, ExecutionResult, ExecutionStatus, Executor, InputValue, Observation,
    ObservationKind, Turn,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

#[derive(Default)]
struct Model(AtomicUsize);

#[async_trait::async_trait]
impl ModelProvider for Model {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ModelResponse::new("r", "ok", Usage::new(1, 1)))
    }
}

struct Exec {
    calls: AtomicUsize,
    status: Mutex<ExecutionStatus>,
    output: &'static str,
}

impl Exec {
    fn new(status: ExecutionStatus, output: &'static str) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            status: Mutex::new(status),
            output,
        })
    }

    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ExecutionResult {
            id: r.id,
            status: *self.status.lock().unwrap(),
            output: self.output.to_string(),
            receipt_id: Some("sha256:test".into()),
            evidence: None,
        })
    }
}

struct Caps;

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut with_input =
            CapabilityDescriptor::new(CapabilityId::new("test.echo")?, "Echo", "Takes a label");
        with_input.inputs.push(CapabilityInput {
            name: "label".into(),
            description: "label".into(),
            required: false,
        });
        Ok(vec![
            CapabilityDescriptor::new(
                CapabilityId::new("compute.selftest")?,
                "Self Test",
                "Deterministic",
            ),
            CapabilityDescriptor::new(CapabilityId::new("test.other")?, "Other", "Another"),
            with_input,
        ])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

fn agent(model: &Arc<Model>, exec: &Arc<Exec>) -> Agent {
    Agent::new(model.clone())
        .with_capabilities(Arc::new(Caps))
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver))
}

fn request(id: &str, capability: &str) -> CapabilityRequest {
    CapabilityRequest::new(ExecutionId::new(id), CapabilityId::new(capability).unwrap())
}

#[tokio::test]
async fn first_execution_records_evidence_and_the_second_reuses_it() {
    let (model, exec) = (
        Arc::new(Model::default()),
        Exec::new(ExecutionStatus::Success, "selftest ok"),
    );
    let a = agent(&model, &exec);

    assert_eq!(
        a.lookup_evidence(&request("e1", "compute.selftest")),
        EvidenceLookup::NotFound
    );
    let first = a
        .obtain_evidence(&request("e1", "compute.selftest"))
        .await
        .unwrap();
    let EvidenceOutcome::Performed { observation, .. } = first else {
        panic!("expected execution")
    };
    assert_eq!(exec.count(), 1);

    // A new request id for the same operation: reused, nothing runs.
    let second = a
        .obtain_evidence(&request("e2", "compute.selftest"))
        .await
        .unwrap();
    assert_eq!(second, EvidenceOutcome::Reused(observation.clone()));
    assert_eq!(exec.count(), 1, "second request must not execute");
    assert_eq!(
        model.0.load(Ordering::SeqCst),
        0,
        "the fast path never calls the model"
    );
    assert_eq!(
        a.lookup_evidence(&request("e3", "compute.selftest")),
        EvidenceLookup::Found(observation)
    );

    let stats = a.evidence_stats();
    assert_eq!((stats.lookups, stats.hits, stats.misses), (4, 2, 2));
}

#[tokio::test]
async fn reuse_adds_no_model_calls_after_a_model_driven_first_request() {
    let (model, exec) = (
        Arc::new(Model::default()),
        Exec::new(ExecutionStatus::Success, "ok"),
    );
    let a = agent(&model, &exec);
    a.turn(Turn::new("run the self test")).await.unwrap(); // the model's one use
    a.obtain_evidence(&request("e1", "compute.selftest"))
        .await
        .unwrap();
    let before = model.0.load(Ordering::SeqCst);
    a.obtain_evidence(&request("e2", "compute.selftest"))
        .await
        .unwrap();
    assert_eq!(model.0.load(Ordering::SeqCst), before);
    assert_eq!(exec.count(), 1);
}

#[tokio::test]
async fn evidence_preserves_reality_exactly() {
    let (model, exec) = (
        Arc::new(Model::default()),
        Exec::new(ExecutionStatus::Success, "selftest failed"),
    );
    let a = agent(&model, &exec);
    a.obtain_evidence(&request("e1", "compute.selftest"))
        .await
        .unwrap();
    let EvidenceLookup::Found(o) = a.lookup_evidence(&request("e2", "compute.selftest")) else {
        panic!()
    };
    assert_eq!(o.kind, ObservationKind::ExecutionCompleted);
    assert_eq!(o.status, ExecutionStatus::Success);
    assert_eq!(o.output.as_deref(), Some("selftest failed"));
    assert_eq!(o.receipt_id.as_deref(), Some("sha256:test"));
    assert_eq!(
        o.execution_id,
        ExecutionId::new("e1"),
        "the original execution id is kept"
    );
}

#[tokio::test]
async fn failure_is_evidence_and_is_not_retried() {
    let (model, exec) = (
        Arc::new(Model::default()),
        Exec::new(ExecutionStatus::Failure, "exit code 3"),
    );
    let a = agent(&model, &exec);
    a.obtain_evidence(&request("e1", "compute.selftest"))
        .await
        .unwrap();
    // Knowledge that it failed is distinct from no knowledge.
    let EvidenceLookup::Found(o) = a.lookup_evidence(&request("e2", "compute.selftest")) else {
        panic!()
    };
    assert_eq!(o.kind, ObservationKind::ExecutionFailed);
    assert_eq!(o.status, ExecutionStatus::Failure);
    let again = a
        .obtain_evidence(&request("e2", "compute.selftest"))
        .await
        .unwrap();
    assert!(matches!(again, EvidenceOutcome::Reused(_)));
    assert_eq!(exec.count(), 1, "no automatic retry");
}

#[tokio::test]
async fn explicit_invalidation_forces_a_fresh_execution() {
    let (model, exec) = (
        Arc::new(Model::default()),
        Exec::new(ExecutionStatus::Success, "ok"),
    );
    let a = agent(&model, &exec);
    let selftest = CapabilityId::new("compute.selftest").unwrap();
    a.obtain_evidence(&request("e1", "compute.selftest"))
        .await
        .unwrap();
    assert!(matches!(
        a.lookup_evidence(&request("x", "compute.selftest")),
        EvidenceLookup::Found(_)
    ));

    assert_eq!(a.invalidate_evidence(&selftest), 1);
    assert_eq!(
        a.lookup_evidence(&request("x", "compute.selftest")),
        EvidenceLookup::NotFound
    );
    assert_eq!(
        a.invalidate_evidence(&selftest),
        0,
        "nothing left to invalidate"
    );

    let outcome = a
        .obtain_evidence(&request("e2", "compute.selftest"))
        .await
        .unwrap();
    assert!(matches!(outcome, EvidenceOutcome::Performed { .. }));
    assert_eq!(exec.count(), 2);
}

#[tokio::test]
async fn invalidation_is_per_capability_and_evidence_does_not_collide() {
    let (model, exec) = (
        Arc::new(Model::default()),
        Exec::new(ExecutionStatus::Success, "ok"),
    );
    let a = agent(&model, &exec);
    a.obtain_evidence(&request("a1", "compute.selftest"))
        .await
        .unwrap();
    assert_eq!(
        a.lookup_evidence(&request("b1", "test.other")),
        EvidenceLookup::NotFound
    );
    a.obtain_evidence(&request("b1", "test.other"))
        .await
        .unwrap();
    assert_eq!(exec.count(), 2);

    let EvidenceLookup::Found(b) = a.lookup_evidence(&request("z", "test.other")) else {
        panic!()
    };
    assert_eq!(b.execution_id, ExecutionId::new("b1"));
    a.invalidate_evidence(&CapabilityId::new("compute.selftest").unwrap());
    assert!(matches!(
        a.lookup_evidence(&request("z", "test.other")),
        EvidenceLookup::Found(_)
    ));
}

#[tokio::test]
async fn different_inputs_have_different_evidence_keys() {
    let x = request("e1", "test.echo").with_input("label", InputValue::Text("x".into()));
    let y = request("e2", "test.echo").with_input("label", InputValue::Text("y".into()));
    let x_again = request("e3", "test.echo").with_input("label", InputValue::Text("x".into()));
    assert_ne!(EvidenceKey::from_request(&x), EvidenceKey::from_request(&y));
    assert_eq!(
        EvidenceKey::from_request(&x),
        EvidenceKey::from_request(&x_again)
    );

    let (model, exec) = (
        Arc::new(Model::default()),
        Exec::new(ExecutionStatus::Success, "ok"),
    );
    let a = agent(&model, &exec);
    a.obtain_evidence(&x).await.unwrap();
    assert_eq!(a.lookup_evidence(&y), EvidenceLookup::NotFound);
    assert!(matches!(
        a.obtain_evidence(&x_again).await.unwrap(),
        EvidenceOutcome::Reused(_)
    ));
    a.obtain_evidence(&y).await.unwrap();
    assert_eq!(exec.count(), 2);
}

#[tokio::test]
async fn a_request_alone_is_never_evidence() {
    let (model, exec) = (
        Arc::new(Model::default()),
        Exec::new(ExecutionStatus::Success, "ok"),
    );
    let a = agent(&model, &exec);
    let req = request("e1", "compute.selftest");
    // Requesting and validating do not create evidence.
    a.validate_capability_request(&req).await.unwrap();
    assert_eq!(a.lookup_evidence(&req), EvidenceLookup::NotFound);

    // An observation of some other execution cannot be recorded for this request.
    let other = Observation {
        execution_id: ExecutionId::new("someone-else"),
        kind: ObservationKind::ExecutionCompleted,
        status: ExecutionStatus::Success,
        output: None,
        receipt_id: None,
        evidence: None,
    };
    assert!(matches!(
        a.record_evidence(&req, &other),
        Err(EvidenceError::ExecutionMismatch(_))
    ));
    assert_eq!(a.lookup_evidence(&req), EvidenceLookup::NotFound);
    assert_eq!(exec.count(), 0);
}

#[tokio::test]
async fn cancelled_and_unexecuted_requests_leave_no_evidence() {
    let (model, exec) = (
        Arc::new(Model::default()),
        Exec::new(ExecutionStatus::Cancelled, ""),
    );
    let a = agent(&model, &exec);
    let req = request("e1", "compute.selftest");
    assert!(matches!(
        a.obtain_evidence(&req).await.unwrap(),
        EvidenceOutcome::Performed { .. }
    ));
    assert_eq!(
        a.lookup_evidence(&req),
        EvidenceLookup::NotFound,
        "a cancelled run proves nothing"
    );

    // Validation failures and executor errors are not evidence either.
    let unknown = request("e2", "no.such");
    assert!(matches!(
        a.obtain_evidence(&unknown).await,
        Err(AgentError::Capability(_))
    ));
    assert_eq!(a.lookup_evidence(&unknown), EvidenceLookup::NotFound);
}

#[tokio::test]
async fn evidence_belongs_to_the_agent_instance() {
    let (model, exec) = (
        Arc::new(Model::default()),
        Exec::new(ExecutionStatus::Success, "ok"),
    );
    let one = agent(&model, &exec);
    let two = agent(&model, &exec);
    one.obtain_evidence(&request("e1", "compute.selftest"))
        .await
        .unwrap();
    assert_eq!(
        two.lookup_evidence(&request("e1", "compute.selftest")),
        EvidenceLookup::NotFound
    );
}
