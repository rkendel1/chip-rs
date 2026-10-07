//! Offline proof for the Ollama adapter: ModelRequest → /api/chat → ModelResponse.
//! No Ollama server is needed; the live run is gated on a configured provider elsewhere.

mod common;

use std::time::Duration;

use common::start;
use fx_core::{FxError, Message, MessageRole, ModelProvider, ModelRequest, Secret};
use fx_provider_http::{HttpProvider, HttpProviderConfig, PROVIDER_OLLAMA, default_endpoint};

/// A representative non-streaming `/api/chat` reply.
const OK_BODY: &str = r#"{"model":"qwen3-coder:latest","created_at":"2025-10-07T10:11:12.123456Z","message":{"role":"assistant","content":"{\"schema\":\"chip.work-decision.v1\",\"decision\":\"request_capability\",\"capability\":\"compute.op_a\"}"},"done":true,"done_reason":"stop","total_duration":1234567890,"load_duration":12345,"prompt_eval_count":229,"prompt_eval_duration":111,"eval_count":48,"eval_duration":222}"#;

/// The mock listens on `http://127.0.0.1:PORT/v1/chat/completions`; Ollama is configured with the
/// server's base URL only.
fn base(url: &str) -> String {
    url.split("/v1/").next().unwrap().to_string()
}

fn config(url: &str) -> HttpProviderConfig {
    HttpProviderConfig::new(PROVIDER_OLLAMA, "qwen3-coder:latest", base(url))
}

fn request() -> ModelRequest {
    ModelRequest::new(
        "qwen3-coder:latest",
        vec![
            Message::new(MessageRole::System, "be brief"),
            Message::new(MessageRole::User, "Hi there"),
        ],
    )
}

#[tokio::test]
async fn builds_the_chat_request_and_maps_the_response() {
    let server = start(200, OK_BODY, Duration::ZERO).await;
    let provider = HttpProvider::new(config(&server.url)).unwrap();

    let response = provider.complete(request()).await.unwrap();
    assert!(response.output.contains("\"capability\":\"compute.op_a\""));
    // The reply text arrives whole and untouched.
    assert_eq!(
        response.output,
        r#"{"schema":"chip.work-decision.v1","decision":"request_capability","capability":"compute.op_a"}"#
    );
    // Ollama has no response id; its creation timestamp stands in.
    assert_eq!(response.id, "2025-10-07T10:11:12.123456Z");
    assert_eq!(response.usage.prompt_tokens, 229);
    assert_eq!(response.usage.completion_tokens, 48);
    assert_eq!(response.usage.total_tokens, 277);

    let captured = server.captured.lock().await;
    assert_eq!(captured.len(), 1, "one request, no retry");
    let req = &captured[0];
    assert!(
        req.request_line.starts_with("POST /api/chat "),
        "{}",
        req.request_line
    );
    let json: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(json["model"], "qwen3-coder:latest");
    assert_eq!(json["stream"], false);
    assert_eq!(json["messages"][0]["role"], "system");
    assert_eq!(json["messages"][0]["content"], "be brief");
    assert_eq!(json["messages"][1]["role"], "user");
    assert_eq!(json["messages"][1]["content"], "Hi there");
    assert_eq!(json["options"]["temperature"], 0.0);
    assert_eq!(json["options"]["num_predict"], 256);
    // No Ollama-specific decision format: the model is not told to emit JSON through the API.
    for absent in ["format", "think", "tools", "keep_alive"] {
        assert!(json.get(absent).is_none(), "unexpected `{absent}`");
    }
}

#[tokio::test]
async fn no_credentials_are_ever_sent_even_when_a_key_is_configured() {
    // A key configured for another provider must not travel to Ollama.
    let server = start(200, OK_BODY, Duration::ZERO).await;
    let cfg = config(&server.url).with_api_key(Secret::new("a-key-meant-for-another-provider"));
    HttpProvider::new(cfg)
        .unwrap()
        .complete(request())
        .await
        .unwrap();
    let captured = server.captured.lock().await;
    assert!(captured[0].header("authorization").is_none());
    assert!(captured[0].header("x-api-key").is_none());
    assert!(
        !captured[0]
            .body
            .contains("a-key-meant-for-another-provider")
    );
    assert!(
        !captured[0]
            .headers
            .iter()
            .any(|(_, v)| v.contains("a-key-meant-for-another-provider"))
    );
}

#[tokio::test]
async fn the_endpoint_may_be_a_base_url_with_a_slash_or_the_full_chat_url() {
    for suffix in ["/", "/api/chat"] {
        let server = start(200, OK_BODY, Duration::ZERO).await;
        let cfg = HttpProviderConfig::new(
            PROVIDER_OLLAMA,
            "m",
            format!("{}{suffix}", base(&server.url)),
        );
        HttpProvider::new(cfg)
            .unwrap()
            .complete(request())
            .await
            .unwrap();
        let line = server.captured.lock().await[0].request_line.clone();
        assert!(line.starts_with("POST /api/chat "), "{suffix}: {line}");
    }
}

#[tokio::test]
async fn max_tokens_and_temperature_come_from_the_request_and_are_omitted_when_unset() {
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
    assert!(json["options"].get("temperature").is_none());
    assert!(json["options"].get("num_predict").is_none());
}

#[tokio::test]
async fn usage_is_reported_only_when_ollama_reports_it() {
    for (body, prompt, completion) in [
        (
            r#"{"created_at":"t","message":{"role":"assistant","content":"x"},"done":true}"#,
            0,
            0,
        ),
        (
            // A cached prompt is not re-evaluated, so Ollama may omit the prompt count.
            r#"{"created_at":"t","message":{"role":"assistant","content":"x"},"done":true,"eval_count":9}"#,
            0,
            9,
        ),
    ] {
        let server = start(200, body, Duration::ZERO).await;
        let response = HttpProvider::new(config(&server.url))
            .unwrap()
            .complete(request())
            .await
            .unwrap();
        assert_eq!(
            (
                response.usage.prompt_tokens,
                response.usage.completion_tokens
            ),
            (prompt, completion)
        );
    }
}

#[tokio::test]
async fn http_errors_stay_errors_with_ollamas_own_message_and_are_not_retried() {
    for (status, body, want) in [
        (
            404,
            r#"{"error":"model 'nope' not found, try pulling it first"}"#,
            "not found, try pulling it first",
        ),
        (
            500,
            r#"{"error":"llama runner process has terminated"}"#,
            "terminated",
        ),
        (503, "overloaded", "no error detail"),
    ] {
        let server = start(status, body, Duration::ZERO).await;
        let err = HttpProvider::new(config(&server.url))
            .unwrap()
            .complete(request())
            .await
            .unwrap_err();
        match err {
            FxError::Provider(m) => {
                assert!(m.contains(&status.to_string()) && m.contains(want), "{m}")
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(server.captured.lock().await.len(), 1, "no retry");
    }
}

#[tokio::test]
async fn an_unauthorized_proxy_is_an_authentication_error() {
    let server = start(401, r#"{"error":"nope"}"#, Duration::ZERO).await;
    let err = HttpProvider::new(config(&server.url))
        .unwrap()
        .complete(request())
        .await
        .unwrap_err();
    assert!(matches!(err, FxError::Authentication(_)), "{err:?}");
}

#[tokio::test]
async fn bad_bodies_are_invalid_responses_never_a_fabricated_decision() {
    for body in [
        "not json",
        "",
        r#"{"created_at":"t","done":true}"#,
        r#"{"created_at":"t","message":{"role":"assistant"},"done":true}"#,
        r#"{"created_at":"t","message":{"role":"assistant","content":""},"done":true}"#,
        r#"{"created_at":"t","message":{"role":"assistant","content":"  \n"},"done":true}"#,
        r#"{"created_at":"t","message":{"role":"assistant","content":null},"done":true}"#,
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
        assert_eq!(server.captured.lock().await.len(), 1, "no retry");
    }
}

#[tokio::test]
async fn an_unreachable_server_is_an_http_error() {
    // Nothing listens here; the connection is refused.
    let cfg = HttpProviderConfig::new(PROVIDER_OLLAMA, "m", "http://127.0.0.1:1");
    let err = HttpProvider::new(cfg)
        .unwrap()
        .complete(request())
        .await
        .unwrap_err();
    assert!(matches!(err, FxError::Http(_)), "{err:?}");
}

#[tokio::test]
async fn a_slow_model_times_out_without_an_answer() {
    let server = start(200, OK_BODY, Duration::from_secs(2)).await;
    let cfg = config(&server.url).with_timeout(Duration::from_millis(100));
    let err = HttpProvider::new(cfg)
        .unwrap()
        .complete(request())
        .await
        .unwrap_err();
    assert!(matches!(err, FxError::Timeout(_)), "{err:?}");
}

#[tokio::test]
async fn a_request_without_messages_is_rejected_before_sending() {
    let server = start(200, OK_BODY, Duration::ZERO).await;
    let err = HttpProvider::new(config(&server.url))
        .unwrap()
        .complete(ModelRequest::new("m", vec![]))
        .await
        .unwrap_err();
    assert!(matches!(err, FxError::InvalidRequest(_)), "{err:?}");
    assert!(server.captured.lock().await.is_empty());
}

#[test]
fn the_endpoint_defaults_to_the_local_server_and_no_key_is_needed() {
    assert_eq!(
        default_endpoint(PROVIDER_OLLAMA),
        Some("http://127.0.0.1:11434")
    );
    // Constructs without an API key.
    assert!(HttpProvider::new(HttpProviderConfig::new(PROVIDER_OLLAMA, "m", "http://x")).is_ok());
}
