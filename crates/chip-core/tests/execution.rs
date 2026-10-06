//! PR3: model turn and execution are independent dependencies coordinated by Chip.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chip_core::{
    Agent, ExecutionError, ExecutionEvent, ExecutionId, ExecutionRequest, ExecutionResult,
    ExecutionStatus, Executor, TestExecutor, Turn,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

struct TestModel;

#[async_trait::async_trait]
impl ModelProvider for TestModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, FxError> {
        Ok(ModelResponse::new("r", "model says hi", Usage::new(1, 1)))
    }
}

struct FailingExecutor;

#[async_trait::async_trait]
impl Executor for FailingExecutor {
    async fn execute(&self, _r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        Err(ExecutionError::ExecutionFailed("boom".into()))
    }
}

/// Records requests; never completes.
struct PendingExecutor {
    dropped: Arc<AtomicBool>,
}

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Executor for PendingExecutor {
    async fn execute(&self, _r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        let _guard = DropFlag(self.dropped.clone());
        std::future::pending().await
    }
}

fn request() -> ExecutionRequest {
    ExecutionRequest::new(ExecutionId::new("exec-1"), "test operation")
}

fn agent() -> Agent {
    Agent::new(Arc::new(TestModel))
}

#[tokio::test]
async fn executor_runs_independently_of_model() {
    let report = agent()
        .with_executor(Arc::new(TestExecutor))
        .execute(request())
        .await;
    let result = report.result.unwrap();
    assert_eq!(result.id, ExecutionId::new("exec-1"));
    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(result.output, "test execution completed");
}

#[tokio::test]
async fn turn_then_execution_returns_structured_result() {
    let agent = agent().with_executor(Arc::new(TestExecutor));
    let out = agent
        .turn_and_execute(Turn::new("Hello"), request())
        .await
        .unwrap();
    assert_eq!(out.turn.response, "model says hi");
    assert_eq!(
        out.execution.result.unwrap().output,
        "test execution completed"
    );
}

#[tokio::test]
async fn successful_execution_event_sequence() {
    let report = agent()
        .with_executor(Arc::new(TestExecutor))
        .execute(request())
        .await;
    let id = ExecutionId::new("exec-1");
    assert_eq!(
        report.events,
        vec![
            ExecutionEvent::ExecutionRequested {
                id: id.clone(),
                intent: "test operation".into()
            },
            ExecutionEvent::ExecutionStarted { id: id.clone() },
            ExecutionEvent::ExecutionCompleted {
                id,
                output: "test execution completed".into()
            },
        ]
    );
}

#[tokio::test]
async fn failure_stays_an_execution_error() {
    let report = agent()
        .with_executor(Arc::new(FailingExecutor))
        .execute(request())
        .await;
    assert_eq!(
        report.result,
        Err(ExecutionError::ExecutionFailed("boom".into()))
    );
    assert!(matches!(
        report.events.last(),
        Some(ExecutionEvent::ExecutionFailed { .. })
    ));

    // A failing executor does not fail the model turn.
    let out = agent()
        .with_executor(Arc::new(FailingExecutor))
        .turn_and_execute(Turn::new("Hello"), request())
        .await
        .unwrap();
    assert_eq!(out.turn.response, "model says hi");
    assert!(out.execution.result.is_err());
}

#[tokio::test]
async fn missing_executor_and_empty_intent_are_reported() {
    let report = agent().execute(request()).await;
    assert!(matches!(
        report.result,
        Err(ExecutionError::ExecutorUnavailable(_))
    ));

    let empty = ExecutionRequest::new(ExecutionId::new("e"), "  ");
    let report = agent()
        .with_executor(Arc::new(TestExecutor))
        .execute(empty)
        .await;
    assert!(matches!(
        report.result,
        Err(ExecutionError::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn failure_status_result_emits_failed_event() {
    struct Reports;
    #[async_trait::async_trait]
    impl Executor for Reports {
        async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
            Ok(ExecutionResult::failure(r.id, "nope"))
        }
    }
    let report = agent()
        .with_executor(Arc::new(Reports))
        .execute(request())
        .await;
    assert_eq!(report.result.unwrap().status, ExecutionStatus::Failure);
    assert!(matches!(
        report.events.last(),
        Some(ExecutionEvent::ExecutionFailed { .. })
    ));
}

#[tokio::test]
async fn dropping_the_future_cancels_pending_execution() {
    let dropped = Arc::new(AtomicBool::new(false));
    let agent = agent().with_executor(Arc::new(PendingExecutor {
        dropped: dropped.clone(),
    }));
    let result = tokio::time::timeout(Duration::from_millis(100), agent.execute(request())).await;
    assert!(
        result.is_err(),
        "pending execution should be cancelled by the timeout"
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "executor future should have been dropped"
    );
}

#[tokio::test]
async fn executor_receives_the_request_unchanged() {
    struct Recording(Mutex<Vec<ExecutionRequest>>);
    #[async_trait::async_trait]
    impl Executor for Recording {
        async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
            self.0.lock().unwrap().push(r.clone());
            Ok(ExecutionResult::success(r.id, "ok"))
        }
    }
    let rec = Arc::new(Recording(Mutex::new(vec![])));
    agent()
        .with_executor(rec.clone())
        .execute(request())
        .await
        .result
        .unwrap();
    assert_eq!(rec.0.lock().unwrap().as_slice(), &[request()]);
}
