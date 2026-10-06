//! PR12: evidence stays valid until the explicit state it was established under changes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, CapabilityRequest, EvidenceLookup, EvidenceOutcome, ExecutionError,
    ExecutionId, ExecutionObserver, ExecutionRequest, ExecutionResult, ExecutionStatus, Executor,
    InputValue, ObservationKind, StateToken, Turn,
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
    status: ExecutionStatus,
}

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(ExecutionResult {
            id: r.id,
            status: self.status,
            output: format!("run {n}"),
            receipt_id: Some(format!("sha256:run-{n}")),
        })
    }
}

struct Caps;

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut echo = CapabilityDescriptor::new(CapabilityId::new("test.echo")?, "Echo", "label");
        echo.inputs.push(chip_core::CapabilityInput {
            name: "label".into(),
            description: "l".into(),
            required: false,
        });
        Ok(vec![
            CapabilityDescriptor::new(CapabilityId::new("cap.a")?, "A", "a"),
            CapabilityDescriptor::new(CapabilityId::new("cap.b")?, "B", "b"),
            echo,
        ])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

struct Rig {
    agent: Agent,
    model: Arc<Model>,
    exec: Arc<Exec>,
}

fn rig(status: ExecutionStatus) -> Rig {
    let model = Arc::new(Model::default());
    let exec = Arc::new(Exec {
        calls: AtomicUsize::new(0),
        status,
    });
    let agent = Agent::new(model.clone())
        .with_capabilities(Arc::new(Caps))
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver));
    Rig { agent, model, exec }
}

impl Rig {
    fn runs(&self) -> usize {
        self.exec.calls.load(Ordering::SeqCst)
    }
}

fn req(id: &str, capability: &str) -> CapabilityRequest {
    CapabilityRequest::new(ExecutionId::new(id), CapabilityId::new(capability).unwrap())
}

fn f1() -> StateToken {
    StateToken::new("F1")
}

fn f2() -> StateToken {
    StateToken::new("F2")
}

#[tokio::test]
async fn same_state_reuses_evidence() {
    let r = rig(ExecutionStatus::Success);
    let first = r
        .agent
        .obtain_evidence_under(&req("e1", "cap.a"), &f1())
        .await
        .unwrap();
    let EvidenceOutcome::Performed { observation, .. } = first else {
        panic!()
    };
    let second = r
        .agent
        .obtain_evidence_under(&req("e2", "cap.a"), &f1())
        .await
        .unwrap();
    assert_eq!(second, EvidenceOutcome::Reused(observation));
    assert_eq!(r.runs(), 1);
    assert_eq!(r.model.0.load(Ordering::SeqCst), 0);
    let stats = r.agent.evidence_stats();
    assert_eq!((stats.hits, stats.stale), (1, 0));
}

#[tokio::test]
async fn changed_state_makes_evidence_stale_not_missing() {
    let r = rig(ExecutionStatus::Success);
    r.agent
        .obtain_evidence_under(&req("e1", "cap.a"), &f1())
        .await
        .unwrap();
    assert_eq!(
        r.agent.lookup_valid_evidence(&req("e2", "cap.a"), &f2()),
        EvidenceLookup::Stale
    );
    // Never-seen evidence is a different answer.
    assert_eq!(
        r.agent.lookup_valid_evidence(&req("e2", "cap.b"), &f2()),
        EvidenceLookup::NotFound
    );
    let stats = r.agent.evidence_stats();
    assert_eq!(
        (stats.hits, stats.stale, stats.misses),
        (0, 1, 2),
        "first obtain missed; cap.b missed"
    );
    assert_eq!(r.runs(), 1, "a lookup never executes");
}

#[tokio::test]
async fn re_execution_establishes_evidence_for_the_new_state() {
    let r = rig(ExecutionStatus::Success);
    r.agent
        .obtain_evidence_under(&req("e1", "cap.a"), &f1())
        .await
        .unwrap();
    let again = r
        .agent
        .obtain_evidence_under(&req("e2", "cap.a"), &f2())
        .await
        .unwrap();
    let EvidenceOutcome::Performed { observation, .. } = again else {
        panic!("stale evidence must re-execute")
    };
    assert_eq!(r.runs(), 2);
    assert_eq!(observation.receipt_id.as_deref(), Some("sha256:run-2"));
    let EvidenceLookup::Found(found) = r.agent.lookup_valid_evidence(&req("e3", "cap.a"), &f2())
    else {
        panic!()
    };
    assert_eq!(found, observation);
    // The F1 evidence was replaced, not kept alongside.
    assert_eq!(
        r.agent.lookup_valid_evidence(&req("e4", "cap.a"), &f1()),
        EvidenceLookup::Stale
    );
}

#[tokio::test]
async fn state_is_per_capability_and_per_inputs() {
    let r = rig(ExecutionStatus::Success);
    r.agent
        .obtain_evidence_under(&req("a1", "cap.a"), &f1())
        .await
        .unwrap();
    // Same token, different capability: no evidence.
    assert_eq!(
        r.agent.lookup_valid_evidence(&req("b1", "cap.b"), &f1()),
        EvidenceLookup::NotFound
    );

    let x = req("x1", "test.echo").with_input("label", InputValue::Text("x".into()));
    let y = req("y1", "test.echo").with_input("label", InputValue::Text("y".into()));
    r.agent.obtain_evidence_under(&x, &f1()).await.unwrap();
    assert_eq!(
        r.agent.lookup_valid_evidence(&y, &f1()),
        EvidenceLookup::NotFound
    );
    assert!(matches!(
        r.agent.lookup_valid_evidence(&x, &f1()),
        EvidenceLookup::Found(_)
    ));
}

#[tokio::test]
async fn failure_evidence_is_reusable_under_its_state_and_not_retried() {
    let r = rig(ExecutionStatus::Failure);
    r.agent
        .obtain_evidence_under(&req("e1", "cap.a"), &f1())
        .await
        .unwrap();
    let EvidenceLookup::Found(o) = r.agent.lookup_valid_evidence(&req("e2", "cap.a"), &f1()) else {
        panic!()
    };
    assert_eq!(o.kind, ObservationKind::ExecutionFailed);
    let again = r
        .agent
        .obtain_evidence_under(&req("e2", "cap.a"), &f1())
        .await
        .unwrap();
    assert!(matches!(again, EvidenceOutcome::Reused(_)));
    assert_eq!(r.runs(), 1, "no automatic retry");

    // Under new state the old failure is stale; looking never retries on its own.
    assert_eq!(
        r.agent.lookup_valid_evidence(&req("e3", "cap.a"), &f2()),
        EvidenceLookup::Stale
    );
    assert_eq!(r.runs(), 1);
}

#[tokio::test]
async fn model_is_not_used_for_freshness() {
    let r = rig(ExecutionStatus::Success);
    r.agent.turn(Turn::new("run it")).await.unwrap();
    let x = r.model.0.load(Ordering::SeqCst);
    r.agent
        .obtain_evidence_under(&req("e1", "cap.a"), &f1())
        .await
        .unwrap();
    r.agent
        .obtain_evidence_under(&req("e2", "cap.a"), &f1())
        .await
        .unwrap();
    assert_eq!(
        r.model.0.load(Ordering::SeqCst),
        x,
        "same state: no added model calls"
    );
    r.agent
        .obtain_evidence_under(&req("e3", "cap.a"), &f2())
        .await
        .unwrap();
    assert_eq!(
        r.model.0.load(Ordering::SeqCst),
        x,
        "state change re-executes without a model"
    );
    assert_eq!(r.runs(), 2);
}

#[tokio::test]
async fn stateless_and_stateful_evidence_never_satisfy_each_other() {
    let r = rig(ExecutionStatus::Success);
    // PR11 behavior is unchanged for stateless evidence.
    r.agent.obtain_evidence(&req("s1", "cap.a")).await.unwrap();
    assert!(matches!(
        r.agent.lookup_evidence(&req("s2", "cap.a")),
        EvidenceLookup::Found(_)
    ));
    assert_eq!(
        r.agent.lookup_valid_evidence(&req("s2", "cap.a"), &f1()),
        EvidenceLookup::Stale
    );

    // Evidence bound to a state is not offered as current without one.
    r.agent
        .obtain_evidence_under(&req("t1", "cap.b"), &f1())
        .await
        .unwrap();
    assert_eq!(
        r.agent.lookup_evidence(&req("t2", "cap.b")),
        EvidenceLookup::Stale
    );
}

#[tokio::test]
async fn a_lookup_never_changes_evidence_and_state_can_return() {
    let r = rig(ExecutionStatus::Success);
    r.agent
        .obtain_evidence_under(&req("e1", "cap.a"), &f1())
        .await
        .unwrap();
    assert_eq!(
        r.agent.lookup_valid_evidence(&req("e2", "cap.a"), &f2()),
        EvidenceLookup::Stale
    );
    // Nothing expired it: when the state matches again the evidence is valid again.
    assert!(matches!(
        r.agent.lookup_valid_evidence(&req("e3", "cap.a"), &f1()),
        EvidenceLookup::Found(_)
    ));
    assert_eq!(r.runs(), 1);
}

#[tokio::test]
async fn recording_under_a_state_still_requires_the_requests_own_execution() {
    let r = rig(ExecutionStatus::Success);
    let other = chip_core::Observation {
        execution_id: ExecutionId::new("someone-else"),
        kind: ObservationKind::ExecutionCompleted,
        status: ExecutionStatus::Success,
        output: None,
        receipt_id: None,
    };
    assert!(
        r.agent
            .record_evidence_under(&req("e1", "cap.a"), &other, &f1())
            .is_err()
    );
    assert_eq!(
        r.agent.lookup_valid_evidence(&req("e1", "cap.a"), &f1()),
        EvidenceLookup::NotFound
    );
}
