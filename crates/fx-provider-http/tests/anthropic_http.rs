//! Offline proof for the Anthropic adapter: ModelRequest → Messages API → ModelResponse.
//! No live call; the live run is credential-gated elsewhere.

mod common;

use std::time::Duration;

use common::start;
use fx_core::{FxError, Message, MessageRole, ModelProvider, ModelRequest, Secret};
use fx_provider_http::{HttpProvider, HttpProviderConfig, PROVIDER_ANTHROPIC, default_endpoint};

const FAKE_KEY: &str = "fake-anthropic-key-for-tests";

const OK_BODY: &str = r#"{"id":"msg_01","type":"message","role":"assistant","model":"m","content":[{"type":"text","text":"{\"decision\":"},{"type":"text","text":"\"complete\"}"}],"stop_reason":"end_turn","usage":{"input_tokens":11,"output_tokens":7}}"#;

fn config(url: &str) -> HttpProviderConfig {
    HttpProviderConfig::new(PROVIDER_ANTHROPIC, "claude-test", url)
        .with_api_key(Secret::new(FAKE_KEY))
}

fn request() -> ModelRequest {
    ModelRequest::new(
        "claude-test",
        vec![
            Message::new(MessageRole::System, "be brief"),
            Message::new(MessageRole::User, "Hi there"),
        ],
    )
}

#[tokio::test]
async fn translates_request_and_response() {
    let server = start(200, OK_BODY, Duration::ZERO).await;
    let provider = HttpProvider::new(config(&server.url)).unwrap();

    let response = provider.complete(request()).await.unwrap();
    assert_eq!(response.id, "msg_01");
    assert_eq!(response.output, r#"{"decision":"complete"}"#);
    assert_eq!(response.usage.prompt_tokens, 11);
    assert_eq!(response.usage.completion_tokens, 7);
    assert_eq!(response.usage.total_tokens, 18);

    let captured = server.captured.lock().await;
    assert_eq!(captured.len(), 1);
    let req = &captured[0];
    assert!(
        req.request_line.starts_with("POST "),
        "{}",
        req.request_line
    );
    assert_eq!(req.header("x-api-key"), Some(FAKE_KEY));
    assert_eq!(req.header("anthropic-version"), Some("2023-06-01"));
    assert!(req.header("authorization").is_none(), "no bearer token");
    let json: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(json["model"], "claude-test");
    assert_eq!(json["max_tokens"], 256);
    assert_eq!(json["system"], "be brief");
    assert_eq!(json["messages"].as_array().unwrap().len(), 1);
    assert_eq!(json["messages"][0]["role"], "user");
    assert_eq!(json["messages"][0]["content"], "Hi there");
    assert!(
        !req.body.contains(FAKE_KEY),
        "the key is a header, never the body"
    );
}

#[tokio::test]
async fn the_workspace_header_is_sent_only_when_configured() {
    let server = start(200, OK_BODY, Duration::ZERO).await;
    HttpProvider::new(config(&server.url))
        .unwrap()
        .complete(request())
        .await
        .unwrap();
    HttpProvider::new(config(&server.url).with_workspace_id("wrkspc_test"))
        .unwrap()
        .complete(request())
        .await
        .unwrap();
    let captured = server.captured.lock().await;
    assert!(captured[0].header("anthropic-workspace-id").is_none());
    assert_eq!(
        captured[1].header("anthropic-workspace-id"),
        Some("wrkspc_test")
    );
}

#[tokio::test]
async fn max_tokens_is_always_sent_and_system_is_omitted_when_absent() {
    let server = start(200, OK_BODY, Duration::ZERO).await;
    let mut request = ModelRequest::new("m", vec![Message::new(MessageRole::User, "hi")]);
    request.max_tokens = None;
    request.temperature = None;
    HttpProvider::new(config(&server.url))
        .unwrap()
        .complete(request)
        .await
        .unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&server.captured.lock().await[0].body).unwrap();
    assert_eq!(json["max_tokens"], 1024);
    assert!(json.get("system").is_none());
    assert!(json.get("temperature").is_none());
}

#[tokio::test]
async fn missing_usage_is_reported_as_zero_not_estimated() {
    let body = r#"{"id":"m","content":[{"type":"text","text":"x"}]}"#;
    let server = start(200, body, Duration::ZERO).await;
    let response = HttpProvider::new(config(&server.url))
        .unwrap()
        .complete(request())
        .await
        .unwrap();
    assert_eq!(response.usage.total_tokens, 0);
}

#[tokio::test]
async fn unauthorized_becomes_authentication_error_without_secret() {
    let body =
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
    let server = start(401, body, Duration::ZERO).await;
    let err = HttpProvider::new(config(&server.url))
        .unwrap()
        .complete(request())
        .await
        .unwrap_err();
    assert!(matches!(err, FxError::Authentication(_)), "{err:?}");
    assert!(!format!("{err} {err:?}").contains(FAKE_KEY));
}

#[tokio::test]
async fn provider_failures_stay_errors_and_never_become_decisions() {
    for (status, body) in [
        (
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}"#,
        ),
        (
            429,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ),
        (
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#,
        ),
    ] {
        let server = start(status, body, Duration::ZERO).await;
        let err = HttpProvider::new(config(&server.url))
            .unwrap()
            .complete(request())
            .await
            .unwrap_err();
        match err {
            FxError::Provider(m) => assert!(m.contains(&status.to_string()), "{m}"),
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn bodies_without_text_are_invalid_responses() {
    for body in [
        "not json",
        r#"{"id":"m","content":[]}"#,
        r#"{"id":"m","content":[{"type":"tool_use","id":"t","name":"n","input":{}}]}"#,
    ] {
        let server = start(200, body, Duration::ZERO).await;
        let err = HttpProvider::new(config(&server.url))
            .unwrap()
            .complete(request())
            .await
            .unwrap_err();
        assert!(
            matches!(err, FxError::InvalidResponse(_)),
            "{body}: {err:?}"
        );
    }
}

#[tokio::test]
async fn a_request_with_only_system_messages_is_rejected_before_sending() {
    let server = start(200, OK_BODY, Duration::ZERO).await;
    let request = ModelRequest::new("m", vec![Message::new(MessageRole::System, "s")]);
    let err = HttpProvider::new(config(&server.url))
        .unwrap()
        .complete(request)
        .await
        .unwrap_err();
    assert!(matches!(err, FxError::InvalidRequest(_)), "{err:?}");
    assert!(server.captured.lock().await.is_empty());
}

#[test]
fn the_endpoint_defaults_for_anthropic_only() {
    assert_eq!(
        default_endpoint(PROVIDER_ANTHROPIC),
        Some("https://api.anthropic.com/v1/messages")
    );
    assert_eq!(default_endpoint("openai-compatible"), None);
}

#[test]
fn debug_output_redacts_api_key() {
    let cfg = config("http://x");
    assert!(!format!("{cfg:?}").contains(FAKE_KEY));
    assert!(!format!("{:?}", HttpProvider::new(cfg).unwrap()).contains(FAKE_KEY));
}
