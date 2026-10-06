//! Integration test proving the complete PR1 path:
//! Turn → Agent → FX ModelProvider → ModelResponse → TurnResult

use std::sync::Arc;

use chip_core::{Agent, Turn, TurnResult};
use fx_core::{FxError, MessageRole, ModelProvider, ModelRequest, ModelResponse, Usage};

#[derive(Default)]
struct IntegrationTestProvider;

#[async_trait::async_trait]
impl ModelProvider for IntegrationTestProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        if request.messages.is_empty() {
            return Err(FxError::InvalidRequest(
                "ModelRequest must contain at least one message".to_string(),
            ));
        }

        Ok(ModelResponse::new(
            "integration-test-response",
            "Integration test provider response",
            Usage::new(10, 15),
        ))
    }
}

#[tokio::test]
async fn complete_path_turn_to_turn_result() {
    let turn = Turn::new("Test message");
    let provider = Arc::new(IntegrationTestProvider);
    let agent = Agent::new(provider);

    let result: TurnResult = agent.turn(turn).await.expect("turn should succeed");

    assert_eq!(result.response, "Integration test provider response");
    assert!(!result.events.is_empty(), "events should not be empty");
    assert_eq!(result.events.len(), 5, "should have exactly 5 events");

    use chip_core::AgentEvent;

    assert!(matches!(result.events[0], AgentEvent::TurnReceived { .. }));
    assert!(matches!(result.events[1], AgentEvent::RequestBuilt { .. }));
    assert!(matches!(result.events[2], AgentEvent::ModelInvoked { .. }));
    assert!(matches!(result.events[3], AgentEvent::ModelResponded { .. }));
    assert!(matches!(result.events[4], AgentEvent::TurnCompleted { .. }));
}

#[tokio::test]
async fn provider_receives_expected_model_request() {
    struct RequestValidatingProvider;

    #[async_trait::async_trait]
    impl ModelProvider for RequestValidatingProvider {
        async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
            assert!(!request.messages.is_empty(), "request must have messages");
            assert_eq!(
                request.messages[0].role,
                MessageRole::User,
                "first message must be from user"
            );
            assert_eq!(
                request.messages[0].content, "Validation test",
                "message content must match"
            );

            Ok(ModelResponse::new(
                "validation-response",
                "Provider received correct request",
                Usage::new(5, 10),
            ))
        }
    }

    let provider = Arc::new(RequestValidatingProvider);
    let agent = Agent::new(provider);

    let result = agent.turn(Turn::new("Validation test")).await;
    assert!(result.is_ok(), "provider validation should pass");
}

#[tokio::test]
async fn provider_error_propagates_to_agent_error() {
    struct FailingProvider;

    #[async_trait::async_trait]
    impl ModelProvider for FailingProvider {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, FxError> {
            Err(FxError::Provider("Simulated provider failure".to_string()))
        }
    }

    let provider = Arc::new(FailingProvider);
    let agent = Agent::new(provider);

    let result = agent.turn(Turn::new("Will fail")).await;
    assert!(
        result.is_err(),
        "provider error should propagate to agent error"
    );
}
