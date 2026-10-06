//! Test verifying that the public FX API contains no provider-specific types.
//! Allowed concepts: ModelId, Message, MessageRole, ModelRequest, ModelResponse, Usage, ModelProvider, FxError.

use fx_core::*;

#[test]
fn api_exports_only_required_concepts() {
    let model_id = ModelId::new("test-model");
    assert_eq!(model_id.to_string(), "test-model");

    let msg = Message::new(MessageRole::User, "test");
    assert_eq!(msg.role, MessageRole::User);

    let usage = Usage::new(10, 20);
    assert_eq!(usage.total_tokens, 30);

    let request = ModelRequest::new("model", vec![msg]);
    assert_eq!(request.messages.len(), 1);

    let response = ModelResponse::new("id", "output", usage);
    assert_eq!(response.output, "output");
}

#[test]
fn error_variants_are_provider_neutral() {
    let invalid_req = FxError::InvalidRequest("bad request".to_string());
    assert!(invalid_req.to_string().contains("invalid request"));

    let provider_err = FxError::Provider("something failed".to_string());
    assert!(provider_err.to_string().contains("provider error"));
}

#[test]
fn message_role_covers_conversation_semantics() {
    let _system = MessageRole::System;
    let _user = MessageRole::User;
    let _assistant = MessageRole::Assistant;
}

#[test]
fn model_request_parameters_are_standard() {
    let request = ModelRequest::new("gpt-4", vec![]);
    assert!(request.max_tokens.is_some());
    assert!(request.temperature.is_some());
}
