//! Offline proof: ModelRequest → HTTP → mock provider → HTTP → ModelResponse.

mod common;

use std::time::Duration;

use common::{OK_BODY, start};
use fx_core::{FxError, Message, MessageRole, ModelProvider, ModelRequest, Secret};
use fx_provider_http::{HttpProvider, HttpProviderConfig, PROVIDER_OPENAI_COMPATIBLE};

const FAKE_KEY: &str = "fake-key-for-tests";

fn config(url: &str) -> HttpProviderConfig {
    HttpProviderConfig::new(PROVIDER_OPENAI_COMPATIBLE, "mock-model", url)
        .with_api_key(Secret::new(FAKE_KEY))
}

fn request() -> ModelRequest {
    ModelRequest::new(
        "mock-model",
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
    assert_eq!(response.id, "resp-1");
    assert_eq!(response.output, "Hello from the mock.");
    assert_eq!(response.usage.prompt_tokens, 3);
    assert_eq!(response.usage.completion_tokens, 5);
    assert_eq!(response.usage.total_tokens, 8);

    let captured = server.captured.lock().await;
    assert_eq!(captured.len(), 1);
    let req = &captured[0];
    assert!(
        req.request_line.starts_with("POST /v1/chat/completions "),
        "{}",
        req.request_line
    );
    assert_eq!(
        req.header("authorization"),
        Some(format!("Bearer {FAKE_KEY}").as_str())
    );
    let json: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(json["model"], "mock-model");
    assert_eq!(json["messages"][0]["role"], "system");
    assert_eq!(json["messages"][0]["content"], "be brief");
    assert_eq!(json["messages"][1]["role"], "user");
    assert_eq!(json["messages"][1]["content"], "Hi there");
}

#[tokio::test]
async fn no_authorization_header_without_api_key() {
    let server = start(200, OK_BODY, Duration::ZERO).await;
    let cfg = HttpProviderConfig::new(PROVIDER_OPENAI_COMPATIBLE, "m", &server.url);
    HttpProvider::new(cfg)
        .unwrap()
        .complete(request())
        .await
        .unwrap();
    assert!(
        server.captured.lock().await[0]
            .header("authorization")
            .is_none()
    );
}

#[tokio::test]
async fn unauthorized_becomes_authentication_error_without_secret() {
    let server = start(401, r#"{"error":{"message":"bad key"}}"#, Duration::ZERO).await;
    let err = HttpProvider::new(config(&server.url))
        .unwrap()
        .complete(request())
        .await
        .unwrap_err();
    assert!(matches!(err, FxError::Authentication(_)), "{err:?}");
    assert!(!format!("{err} {err:?}").contains(FAKE_KEY));
}

#[tokio::test]
async fn provider_error_becomes_fx_error() {
    let server = start(
        500,
        r#"{"error":{"message":"model exploded"}}"#,
        Duration::ZERO,
    )
    .await;
    let err = HttpProvider::new(config(&server.url))
        .unwrap()
        .complete(request())
        .await
        .unwrap_err();
    match err {
        FxError::Provider(m) => assert!(m.contains("model exploded"), "{m}"),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn malformed_body_is_invalid_response() {
    let server = start(200, "not json", Duration::ZERO).await;
    let err = HttpProvider::new(config(&server.url))
        .unwrap()
        .complete(request())
        .await
        .unwrap_err();
    assert!(matches!(err, FxError::InvalidResponse(_)), "{err:?}");

    let server = start(200, r#"{"choices":[]}"#, Duration::ZERO).await;
    let err = HttpProvider::new(config(&server.url))
        .unwrap()
        .complete(request())
        .await
        .unwrap_err();
    assert!(matches!(err, FxError::InvalidResponse(_)), "{err:?}");
}

#[tokio::test]
async fn unreachable_endpoint_is_http_error() {
    let err = HttpProvider::new(config("http://127.0.0.1:1/v1"))
        .unwrap()
        .complete(request())
        .await
        .unwrap_err();
    assert!(matches!(err, FxError::Http(_)), "{err:?}");
    assert!(!format!("{err}").contains(FAKE_KEY));
}

#[tokio::test]
async fn slow_endpoint_times_out() {
    let server = start(200, OK_BODY, Duration::from_secs(5)).await;
    let cfg = config(&server.url).with_timeout(Duration::from_millis(200));
    let err = HttpProvider::new(cfg)
        .unwrap()
        .complete(request())
        .await
        .unwrap_err();
    assert!(matches!(err, FxError::Timeout(_)), "{err:?}");
}

#[tokio::test]
async fn dropping_the_future_cancels_the_request() {
    let server = start(200, OK_BODY, Duration::from_secs(5)).await;
    let provider = HttpProvider::new(config(&server.url)).unwrap();
    let started = std::time::Instant::now();
    let result =
        tokio::time::timeout(Duration::from_millis(200), provider.complete(request())).await;
    assert!(result.is_err(), "outer timeout should cancel the call");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn configuration_errors() {
    let bad_provider = HttpProviderConfig::new("nope", "m", "http://x");
    assert!(matches!(
        HttpProvider::new(bad_provider),
        Err(FxError::Configuration(_))
    ));
    let no_endpoint = HttpProviderConfig::new(PROVIDER_OPENAI_COMPATIBLE, "m", " ");
    assert!(matches!(
        HttpProvider::new(no_endpoint),
        Err(FxError::Configuration(_))
    ));
}

#[test]
fn debug_output_redacts_api_key() {
    let cfg = config("http://x");
    assert!(!format!("{cfg:?}").contains(FAKE_KEY));
    let provider = HttpProvider::new(cfg).unwrap();
    assert!(!format!("{provider:?}").contains(FAKE_KEY));
}

/// Chip sends each observation as its own system message. Some servers' chat templates accept a
/// system message only first, and only one: consecutive system messages go out as one, in order,
/// and nothing else about the conversation changes.
#[tokio::test]
async fn consecutive_system_messages_are_sent_as_one_in_order() {
    let server = start(200, OK_BODY, Duration::ZERO).await;
    let provider = HttpProvider::new(config(&server.url)).unwrap();
    let request = ModelRequest::new(
        "mock-model",
        vec![
            Message::new(MessageRole::System, "observation one"),
            Message::new(MessageRole::System, "observation two"),
            Message::new(MessageRole::System, "observation three"),
            Message::new(MessageRole::User, "decide"),
            Message::new(MessageRole::Assistant, "ok"),
            Message::new(MessageRole::System, "a later system note"),
            Message::new(MessageRole::User, "again"),
        ],
    );
    provider.complete(request).await.unwrap();
    let captured = server.captured.lock().await;
    let json: serde_json::Value = serde_json::from_str(&captured[0].body).unwrap();
    let roles: Vec<&str> = json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "system", "user"]);
    assert_eq!(
        json["messages"][0]["content"],
        "observation one\n\nobservation two\n\nobservation three"
    );
    assert_eq!(json["messages"][1]["content"], "decide");
    assert_eq!(
        json["messages"][3]["content"], "a later system note",
        "a system message after a user turn stays where it is"
    );
}

// ---- structured output and thinking, requested by the provider boundary ------------------------------------------

const QWEN: &str = "mlx-community/Qwen3.5-35B-A3B-4bit";

async fn sent(config: HttpProviderConfig, server: &common::MockServer) -> serde_json::Value {
    HttpProvider::new(config)
        .unwrap()
        .complete(ModelRequest::new(
            QWEN,
            vec![Message::new(MessageRole::User, "Decide.")],
        ))
        .await
        .unwrap();
    let captured = server.captured.lock().await;
    assert_eq!(captured.len(), 1, "exactly one request");
    serde_json::from_str(&captured[0].body).unwrap()
}

#[tokio::test]
async fn the_request_asks_for_a_json_object_and_no_thinking_when_configured() {
    let server = start(200, OK_BODY, Duration::ZERO).await;
    let cfg = HttpProviderConfig::new(PROVIDER_OPENAI_COMPATIBLE, QWEN, &server.url)
        .with_json_object_output()
        .with_enable_thinking(false);
    let body = sent(cfg, &server).await;
    assert_eq!(
        body["response_format"],
        serde_json::json!({"type": "json_object"})
    );
    // Servers read this from the top level of the body; a literal `extra_body` member is ignored by
    // them, so it is not sent.
    assert_eq!(
        body["chat_template_kwargs"],
        serde_json::json!({"enable_thinking": false})
    );
    assert!(body.get("extra_body").is_none());
    // A request field, not prompt text: the messages are exactly what the caller gave.
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(body["messages"][0]["content"], "Decide.");
    assert!(!body["messages"].to_string().contains("think"));
}

#[tokio::test]
async fn nothing_extra_is_sent_unless_configured() {
    let server = start(200, OK_BODY, Duration::ZERO).await;
    let cfg = HttpProviderConfig::new(PROVIDER_OPENAI_COMPATIBLE, QWEN, &server.url);
    let body = sent(cfg, &server).await;
    for key in ["response_format", "chat_template_kwargs", "extra_body"] {
        assert!(
            body.get(key).is_none(),
            "{key} is sent only when configured: other OpenAI-compatible servers see the plain request"
        );
    }
}

#[tokio::test]
async fn thinking_can_be_asked_for_explicitly_too() {
    let server = start(200, OK_BODY, Duration::ZERO).await;
    let cfg = HttpProviderConfig::new(PROVIDER_OPENAI_COMPATIBLE, QWEN, &server.url)
        .with_enable_thinking(true);
    let body = sent(cfg, &server).await;
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    assert!(body.get("response_format").is_none());
}

#[tokio::test]
async fn the_selected_model_and_endpoint_are_used_unchanged() {
    let server = start(200, OK_BODY, Duration::ZERO).await;
    let cfg = HttpProviderConfig::new(PROVIDER_OPENAI_COMPATIBLE, QWEN, &server.url)
        .with_json_object_output();
    let body = sent(cfg, &server).await;
    assert_eq!(body["model"], QWEN);
    let captured = server.captured.lock().await;
    assert!(
        captured[0]
            .request_line
            .starts_with("POST /v1/chat/completions ")
    );
}

#[tokio::test]
async fn the_other_adapters_never_send_these_fields() {
    for provider in ["ollama", "anthropic"] {
        let server = start(200, OK_BODY, Duration::ZERO).await;
        let cfg = HttpProviderConfig::new(provider, QWEN, &server.url)
            .with_json_object_output()
            .with_enable_thinking(false);
        let _ = HttpProvider::new(cfg)
            .unwrap()
            .complete(ModelRequest::new(
                QWEN,
                vec![Message::new(MessageRole::User, "x")],
            ))
            .await;
        let captured = server.captured.lock().await;
        let body = &captured[0].body;
        assert!(
            !body.contains("response_format") && !body.contains("chat_template_kwargs"),
            "{provider}: {body}"
        );
    }
}

#[tokio::test]
async fn a_failed_request_stays_failed_with_no_second_attempt() {
    let server = start(
        500,
        r#"{"error":{"message":"out of memory"}}"#,
        Duration::ZERO,
    )
    .await;
    let cfg = HttpProviderConfig::new(PROVIDER_OPENAI_COMPATIBLE, QWEN, &server.url)
        .with_json_object_output()
        .with_enable_thinking(false);
    let err = HttpProvider::new(cfg)
        .unwrap()
        .complete(ModelRequest::new(
            QWEN,
            vec![Message::new(MessageRole::User, "x")],
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, FxError::Provider(_)), "{err:?}");
    assert_eq!(server.captured.lock().await.len(), 1, "no retry");
}

#[tokio::test]
async fn a_reply_with_reasoning_before_the_json_is_returned_exactly_as_it_came() {
    // The provider boundary does not repair, strip or extract: what the server said is the output.
    let reply = "Thinking Process:\n1. read the file\n{\"decision\":\"block\",\"reason\":\"x\"}";
    let body = serde_json::json!({
        "id": "r", "choices": [{"message": {"role": "assistant", "content": reply}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    })
    .to_string();
    let server = start(200, &body, Duration::ZERO).await;
    let cfg = HttpProviderConfig::new(PROVIDER_OPENAI_COMPATIBLE, QWEN, &server.url)
        .with_json_object_output();
    let out = HttpProvider::new(cfg)
        .unwrap()
        .complete(ModelRequest::new(
            QWEN,
            vec![Message::new(MessageRole::User, "x")],
        ))
        .await
        .unwrap();
    assert_eq!(out.output, reply);
}
