//! Test proving a provider can be implemented without modifying Chip.
//! This demonstrates that Chip operates against the trait, not a concrete implementation.

use fx_core::{
    FxError, Message, MessageRole, ModelId, ModelProvider, ModelRequest, ModelResponse, Usage,
};

#[derive(Default)]
struct FirstAlternativeProvider;

#[async_trait::async_trait]
impl ModelProvider for FirstAlternativeProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        let response_text = format!(
            "FirstAlternativeProvider processed {} messages",
            request.messages.len()
        );

        Ok(ModelResponse::new(
            "first-provider-1",
            response_text,
            Usage::new(1, 1),
        ))
    }
}

#[derive(Default)]
struct SecondAlternativeProvider;

#[async_trait::async_trait]
impl ModelProvider for SecondAlternativeProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        if request.model != ModelId::new("test-model") {
            return Err(FxError::InvalidRequest(
                "Only test-model is supported".to_string(),
            ));
        }

        Ok(ModelResponse::new(
            "second-provider-2",
            "SecondAlternativeProvider: Ready",
            Usage::new(2, 2),
        ))
    }
}

#[tokio::test]
async fn first_alternative_provider_implements_trait() {
    let provider = FirstAlternativeProvider;
    let request = ModelRequest::new("test", vec![Message::new(MessageRole::User, "Hello")]);

    let response = provider.complete(request).await;
    assert!(
        response.is_ok(),
        "first provider should complete successfully"
    );

    let result = response.unwrap();
    assert!(result.output.contains("FirstAlternativeProvider"));
}

#[tokio::test]
async fn second_alternative_provider_implements_trait() {
    let provider = SecondAlternativeProvider;
    let request = ModelRequest::new("test-model", vec![Message::new(MessageRole::User, "Hello")]);

    let response = provider.complete(request).await;
    assert!(
        response.is_ok(),
        "second provider should complete successfully"
    );

    let result = response.unwrap();
    assert!(result.output.contains("SecondAlternativeProvider"));
}

#[tokio::test]
async fn different_providers_work_interchangeably() {
    let request = ModelRequest::new("test-model", vec![Message::new(MessageRole::User, "Test")]);

    let first_result = FirstAlternativeProvider
        .complete(request.clone())
        .await
        .unwrap();
    assert!(first_result.output.contains("FirstAlternativeProvider"));

    let second_result = SecondAlternativeProvider.complete(request).await.unwrap();
    assert!(second_result.output.contains("SecondAlternativeProvider"));
}
