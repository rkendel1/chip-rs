use std::error::Error;
use std::fmt;
use std::sync::Arc;

use fx_core::{Message, MessageRole, ModelProvider, ModelRequest, ModelResponse};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub user_message: String,
}

impl Turn {
    pub fn new(user_message: impl Into<String>) -> Self {
        Self {
            user_message: user_message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    TurnReceived { message: String },
    RequestBuilt { model: String },
    ModelInvoked { model: String },
    ModelResponded { output: String },
    TurnCompleted { response: String },
    Execution(ExecutionEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnResult {
    pub response: String,
    pub events: Vec<AgentEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    InvalidTurn(String),
    Provider(String),
}

/// Identifier correlating an execution request, its events and its result.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExecutionId(pub String);

impl ExecutionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A semantic request to perform work. `intent` names the operation; it is
/// not a command line and implies nothing about how it is carried out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionRequest {
    pub id: ExecutionId,
    pub intent: String,
}

impl ExecutionRequest {
    pub fn new(id: ExecutionId, intent: impl Into<String>) -> Self {
        Self {
            id,
            intent: intent.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStatus {
    Success,
    Failure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionResult {
    pub id: ExecutionId,
    pub status: ExecutionStatus,
    pub output: String,
    /// Stable identifier of the executor's evidence for this execution, if it
    /// provides one. Chip preserves it and does not interpret it.
    pub receipt_id: Option<String>,
}

impl ExecutionResult {
    pub fn success(id: ExecutionId, output: impl Into<String>) -> Self {
        Self {
            id,
            status: ExecutionStatus::Success,
            output: output.into(),
            receipt_id: None,
        }
    }

    pub fn failure(id: ExecutionId, output: impl Into<String>) -> Self {
        Self {
            id,
            status: ExecutionStatus::Failure,
            output: output.into(),
            receipt_id: None,
        }
    }

    pub fn with_receipt_id(mut self, receipt_id: impl Into<String>) -> Self {
        self.receipt_id = Some(receipt_id.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionError {
    InvalidRequest(String),
    ExecutorUnavailable(String),
    ExecutionFailed(String),
    Cancelled,
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(f, "invalid execution request: {message}"),
            Self::ExecutorUnavailable(message) => write!(f, "executor unavailable: {message}"),
            Self::ExecutionFailed(message) => write!(f, "execution failed: {message}"),
            Self::Cancelled => write!(f, "execution cancelled"),
        }
    }
}

impl Error for ExecutionError {}

/// Semantic execution events: identifiers and descriptions only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionEvent {
    ExecutionRequested { id: ExecutionId, intent: String },
    ExecutionStarted { id: ExecutionId },
    ExecutionCompleted { id: ExecutionId, output: String },
    ExecutionFailed { id: ExecutionId, reason: String },
}

/// Performs work on behalf of Chip. Implementations are injected; dropping the
/// returned future stops waiting for the result.
#[async_trait::async_trait]
pub trait Executor: Send + Sync {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError>;
}

/// Deterministic executor for tests and demos. Performs no real work.
#[derive(Debug, Default, Clone, Copy)]
pub struct TestExecutor;

#[async_trait::async_trait]
impl Executor for TestExecutor {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        Ok(ExecutionResult::success(
            request.id,
            "test execution completed",
        ))
    }
}

/// Outcome of one execution request, with the events it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionReport {
    pub result: Result<ExecutionResult, ExecutionError>,
    pub events: Vec<ExecutionEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResult {
    pub turn: TurnResult,
    pub execution: ExecutionReport,
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTurn(message) => write!(f, "invalid turn: {message}"),
            Self::Provider(message) => write!(f, "provider error: {message}"),
        }
    }
}

impl Error for AgentError {}

const DEFAULT_MODEL: &str = "chip-test-model";

pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    model: String,
    executor: Option<Arc<dyn Executor>>,
}

impl Agent {
    pub fn new(provider: Arc<dyn ModelProvider>) -> Self {
        Self::with_model(provider, DEFAULT_MODEL)
    }

    pub fn with_model(provider: Arc<dyn ModelProvider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            executor: None,
        }
    }

    /// Injects an executor, independent of the model provider.
    pub fn with_executor(mut self, executor: Arc<dyn Executor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Asks the injected executor to perform `request`. Failures stay
    /// execution errors; they are reported, not converted to model errors.
    pub async fn execute(&self, request: ExecutionRequest) -> ExecutionReport {
        let id = request.id.clone();
        let mut events = vec![ExecutionEvent::ExecutionRequested {
            id: id.clone(),
            intent: request.intent.clone(),
        }];

        let outcome = if request.intent.trim().is_empty() {
            Err(ExecutionError::InvalidRequest(
                "intent cannot be empty".to_string(),
            ))
        } else if let Some(executor) = &self.executor {
            events.push(ExecutionEvent::ExecutionStarted { id: id.clone() });
            executor.execute(request).await
        } else {
            Err(ExecutionError::ExecutorUnavailable(
                "no executor injected".to_string(),
            ))
        };

        match &outcome {
            Ok(result) if result.status == ExecutionStatus::Success => {
                events.push(ExecutionEvent::ExecutionCompleted {
                    id,
                    output: result.output.clone(),
                });
            }
            Ok(result) => events.push(ExecutionEvent::ExecutionFailed {
                id,
                reason: result.output.clone(),
            }),
            Err(error) => events.push(ExecutionEvent::ExecutionFailed {
                id,
                reason: error.to_string(),
            }),
        }

        ExecutionReport {
            result: outcome,
            events,
        }
    }

    /// Runs a model turn, then the explicitly supplied execution request.
    /// The caller decides what to execute; the model output is not parsed.
    pub async fn turn_and_execute(
        &self,
        turn: Turn,
        request: ExecutionRequest,
    ) -> Result<AgentResult, AgentError> {
        let turn = self.turn(turn).await?;
        let execution = self.execute(request).await;
        Ok(AgentResult { turn, execution })
    }

    pub async fn turn(&self, turn: Turn) -> Result<TurnResult, AgentError> {
        if turn.user_message.trim().is_empty() {
            return Err(AgentError::InvalidTurn(
                "turn message cannot be empty".to_string(),
            ));
        }

        let request = ModelRequest::new(
            self.model.clone(),
            vec![Message::new(MessageRole::User, turn.user_message.clone())],
        );

        let response = self
            .provider
            .complete(request.clone())
            .await
            .map_err(|error| AgentError::Provider(error.to_string()))?;

        let events = vec![
            AgentEvent::TurnReceived {
                message: turn.user_message.clone(),
            },
            AgentEvent::RequestBuilt {
                model: request.model.to_string(),
            },
            AgentEvent::ModelInvoked {
                model: request.model.to_string(),
            },
            AgentEvent::ModelResponded {
                output: response.output.clone(),
            },
            AgentEvent::TurnCompleted {
                response: response.output.clone(),
            },
        ];

        Ok(TurnResult {
            response: response.output,
            events,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Agent, Turn};
    use std::sync::Arc;

    use fx_core::{
        FxError, Message, MessageRole, ModelProvider, ModelRequest, ModelResponse, Usage,
    };

    #[derive(Default)]
    struct TestProvider;

    #[async_trait::async_trait]
    impl ModelProvider for TestProvider {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, FxError> {
            Ok(ModelResponse::new(
                "test-response",
                "Hello from the test provider.",
                Usage::new(4, 6),
            ))
        }
    }

    #[tokio::test]
    async fn agent_turn_returns_response_and_events() {
        let agent = Agent::new(Arc::new(TestProvider));
        let result = agent.turn(Turn::new("Hello")).await.unwrap();

        assert_eq!(result.response, "Hello from the test provider.");
        assert_eq!(result.events.len(), 5);
        assert!(matches!(
            result.events[0],
            crate::AgentEvent::TurnReceived { ref message } if message == "Hello"
        ));
    }
}
