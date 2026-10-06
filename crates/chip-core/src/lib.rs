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
    Capability(CapabilityEvent),
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

/// Stable, opaque name of a declared ability (for example `vendor.operation`).
/// It is a semantic name, never a command: only lowercase letters, digits,
/// `.`, `_` and `-` are accepted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CapabilityId(String);

impl CapabilityId {
    pub fn new(id: impl Into<String>) -> Result<Self, CapabilityError> {
        let id = id.into();
        let valid = !id.is_empty()
            && id.len() <= 128
            && id.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
            });
        if valid {
            Ok(Self(id))
        } else {
            Err(CapabilityError::InvalidId(id))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One named input a capability accepts. A deliberately small input contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityInput {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// What a capability is, with no implementation details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityDescriptor {
    pub id: CapabilityId,
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub inputs: Vec<CapabilityInput>,
}

impl CapabilityDescriptor {
    pub fn new(id: CapabilityId, name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            description: description.into(),
            version: None,
            inputs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityAvailability {
    Available,
    Unavailable(String),
    Misconfigured(String),
}

/// A declared ability together with whether it can currently be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub descriptor: CapabilityDescriptor,
    pub availability: CapabilityAvailability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityError {
    InvalidId(String),
    Unknown(String),
    Unavailable(String),
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId(id) => write!(f, "invalid capability id: {id:?}"),
            Self::Unknown(id) => write!(f, "unknown capability: {id}"),
            Self::Unavailable(message) => write!(f, "capabilities unavailable: {message}"),
        }
    }
}

impl Error for CapabilityError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityEvent {
    CapabilitiesRequested,
    CapabilitiesAvailable { count: usize },
    CapabilitiesUnavailable { reason: String },
}

/// Describes what is available. Discovery only: it never performs work, and
/// execution stays with `Executor`.
#[async_trait::async_trait]
pub trait CapabilityProvider: Send + Sync {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError>;

    /// Whether a described capability can currently be used. Must not perform it.
    async fn availability(&self, id: &CapabilityId) -> CapabilityAvailability;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityReport {
    pub result: Result<Vec<Capability>, CapabilityError>,
    pub events: Vec<CapabilityEvent>,
}

impl ExecutionRequest {
    /// The request that asks an executor to perform a declared capability.
    pub fn for_capability(id: ExecutionId, capability: &CapabilityId) -> Self {
        Self::new(id, capability.as_str())
    }
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
    capabilities: Option<Arc<dyn CapabilityProvider>>,
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
            capabilities: None,
        }
    }

    /// Optionally injects a source of capability descriptors.
    pub fn with_capabilities(mut self, provider: Arc<dyn CapabilityProvider>) -> Self {
        self.capabilities = Some(provider);
        self
    }

    /// Asks the injected provider what is available. Only describes; nothing is
    /// selected or executed.
    pub async fn discover_capabilities(&self) -> CapabilityReport {
        let mut events = vec![CapabilityEvent::CapabilitiesRequested];
        let result = match &self.capabilities {
            None => Err(CapabilityError::Unavailable(
                "no capability provider injected".to_string(),
            )),
            Some(provider) => match provider.capabilities().await {
                Err(error) => Err(error),
                Ok(descriptors) => {
                    let mut found = Vec::with_capacity(descriptors.len());
                    for descriptor in descriptors {
                        let availability = provider.availability(&descriptor.id).await;
                        found.push(Capability {
                            descriptor,
                            availability,
                        });
                    }
                    Ok(found)
                }
            },
        };
        events.push(match &result {
            Ok(found) => CapabilityEvent::CapabilitiesAvailable { count: found.len() },
            Err(error) => CapabilityEvent::CapabilitiesUnavailable {
                reason: error.to_string(),
            },
        });
        CapabilityReport { result, events }
    }

    /// Turns an explicitly chosen, declared and available capability into an
    /// execution request. Unknown or unusable capabilities fail here, before
    /// any executor is involved.
    pub async fn request_for_capability(
        &self,
        id: ExecutionId,
        capability: &CapabilityId,
    ) -> Result<ExecutionRequest, CapabilityError> {
        let provider = self.capabilities.as_ref().ok_or_else(|| {
            CapabilityError::Unavailable("no capability provider injected".to_string())
        })?;
        let declared = provider.capabilities().await?;
        if !declared.iter().any(|d| &d.id == capability) {
            return Err(CapabilityError::Unknown(capability.to_string()));
        }
        match provider.availability(capability).await {
            CapabilityAvailability::Available => {
                Ok(ExecutionRequest::for_capability(id, capability))
            }
            CapabilityAvailability::Unavailable(reason)
            | CapabilityAvailability::Misconfigured(reason) => {
                Err(CapabilityError::Unavailable(reason))
            }
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
