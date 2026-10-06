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

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTurn(message) => write!(f, "invalid turn: {message}"),
            Self::Provider(message) => write!(f, "provider error: {message}"),
        }
    }
}

impl Error for AgentError {}

pub struct Agent {
    provider: Arc<dyn ModelProvider>,
}

impl Agent {
    pub fn new(provider: Arc<dyn ModelProvider>) -> Self {
        Self { provider }
    }

    pub async fn turn(&self, turn: Turn) -> Result<TurnResult, AgentError> {
        if turn.user_message.trim().is_empty() {
            return Err(AgentError::InvalidTurn("turn message cannot be empty".to_string()));
        }

        let request = ModelRequest::new(
            "chip-test-model",
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
