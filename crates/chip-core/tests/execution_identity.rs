//! Execution identity is Chip's. A provider's response id is optional correlation metadata: it can
//! repeat, be absent, or be hostile, and no execution takes its identity from it.
//!
//! Deterministic and offline: a scripted model (always answering with the same provider response
//! id, or none) goes through the real `ModelDecisionBoundary` and work loop; a recording executor
//! stands in for Compute.

use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, ExecutionError, ExecutionEvent, ExecutionObserver, ExecutionRequest,
    ExecutionResult, Executor, LocalWorkPolicy, ModelDecisionBoundary, ObservationKind,
    WorkDecision, WorkEvent, WorkGoal, WorkId, WorkOutcome, WorkReport, WorkSpec, WorkView,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

/// Answers every call with a capability request and the same provider response id.
struct Model(&'static str, Mutex<usize>);

#[async_trait::async_trait]
impl ModelProvider for Model {
    async fn complete(&self, _: ModelRequest) -> Result<ModelResponse, FxError> {
        let mut n = self.1.lock().unwrap();
        *n += 1;
        let capability = if *n % 2 == 1 {
            "compute.selftest"
        } else {
            "compute.info"
        };
        Ok(ModelResponse::new(
            self.0,
            format!(r#"{{"decision":"request_capability","capability":"{capability}"}}"#),
            Usage::new(1, 1),
        ))
    }
}

struct Exec(Mutex<Vec<String>>);

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.0.lock().unwrap().push(r.id.to_string());
        Ok(ExecutionResult::success(r.id, "ok").with_receipt_id("receipt-from-executor"))
    }
}

struct Caps;

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(vec![
            CapabilityDescriptor::new(
                CapabilityId::new("compute.selftest")?,
                "selftest",
                "Deterministic test",
            ),
            CapabilityDescriptor::new(
                CapabilityId::new("compute.info")?,
                "info",
                "Runtime information",
            ),
        ])
    }
    async fn availability(&self, _: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

/// Complete once two executions have been observed.
struct AfterTwo;

impl LocalWorkPolicy for AfterTwo {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        (view
            .observations
            .iter()
            .filter(|o| o.kind == ObservationKind::ExecutionCompleted)
            .count()
            >= 2)
            .then(|| WorkDecision::Complete {
                summary: "two executions observed".into(),
            })
    }
}

async fn run(provider_id: &'static str, work: &str) -> (WorkReport, Vec<String>) {
    let exec = Arc::new(Exec(Mutex::new(vec![])));
    let agent = Agent::new(Arc::new(Model(provider_id, Mutex::new(0))))
        .with_capabilities(Arc::new(Caps))
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver));
    let spec = WorkSpec::new(WorkId::new(work), WorkGoal::new("two selftests"));
    let report = agent
        .run_work(&spec, &AfterTwo, &ModelDecisionBoundary)
        .await;
    let seen = exec.0.lock().unwrap().clone();
    (report, seen)
}

fn started(report: &WorkReport) -> Vec<String> {
    report
        .events
        .iter()
        .filter_map(|e| match e {
            WorkEvent::Execution(ExecutionEvent::ExecutionStarted { id }) => Some(id.to_string()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn two_executions_with_the_same_provider_response_id_get_different_chip_ids() {
    let (report, seen) = run("resp-same", "w").await;
    assert!(
        matches!(report.outcome, WorkOutcome::Completed { .. }),
        "{:?}",
        report.outcome
    );

    let ids = started(&report);
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    assert!(ids.iter().all(|id| id.starts_with("w-exec-")), "{ids:?}");
    assert!(ids.iter().all(|id| !id.contains("resp-same")), "{ids:?}");

    // The provider's id is still there, as metadata, identical for both.
    let provider: Vec<_> = report
        .origins
        .iter()
        .map(|o| o.provider_response_id.as_deref())
        .collect();
    assert_eq!(provider, [Some("resp-same"), Some("resp-same")]);

    // One execution, one id: Compute was asked with it and the observations carry it.
    assert_eq!(seen, ids);
    let observed: Vec<_> = report
        .observations
        .iter()
        .map(|o| o.execution_id.to_string())
        .collect();
    assert_eq!(observed, ids);
}

#[tokio::test]
async fn an_absent_provider_response_id_is_valid() {
    let (report, _) = run("", "w").await;
    assert!(
        matches!(report.outcome, WorkOutcome::Completed { .. }),
        "{:?}",
        report.outcome
    );
    assert_eq!(started(&report), ["w-exec-1", "w-exec-2"]);
    assert!(
        report
            .origins
            .iter()
            .all(|o| o.provider_response_id.is_none())
    );
}

#[tokio::test]
async fn one_execution_keeps_one_id_through_every_event() {
    let (report, _) = run("resp-same", "w").await;
    for n in 1..=2 {
        let id = format!("w-exec-{n}");
        let mut lifecycle = Vec::new();
        for e in &report.events {
            if let WorkEvent::Execution(e) = e {
                let (kind, eid) = match e {
                    ExecutionEvent::ExecutionRequested { id, .. } => ("requested", id),
                    ExecutionEvent::ExecutionStarted { id } => ("started", id),
                    ExecutionEvent::ExecutionCompleted { id, .. } => ("completed", id),
                    ExecutionEvent::ExecutionFailed { id, .. } => ("failed", id),
                };
                if eid.to_string() == id {
                    lifecycle.push(kind);
                }
            }
        }
        assert_eq!(lifecycle, ["requested", "started", "completed"], "{id}");
        assert_eq!(
            report
                .observations
                .iter()
                .filter(|o| o.execution_id.to_string() == id)
                .count(),
            1,
            "{id}"
        );
    }
}

#[tokio::test]
async fn hostile_provider_ids_never_become_identity() {
    for hostile in ["../../etc/passwd", "a b; rm -rf /", "\u{1F4A5}", "w-exec-1"] {
        let (report, _) = run(hostile, "w").await;
        assert_eq!(started(&report), ["w-exec-1", "w-exec-2"], "{hostile:?}");
    }
}

#[tokio::test]
async fn ids_are_per_work_so_two_works_do_not_share_a_counter() {
    let (a, _) = run("resp-same", "wa").await;
    let (b, _) = run("resp-same", "wb").await;
    assert_eq!(started(&a), ["wa-exec-1", "wa-exec-2"]);
    assert_eq!(started(&b), ["wb-exec-1", "wb-exec-2"]);
}
