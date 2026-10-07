//! Adapter for a local Ollama server (`POST /api/chat`, non-streaming).
//! All JSON structures here are private to this module.
//!
//! `HttpProviderConfig::endpoint` is the server's base URL (`http://127.0.0.1:11434`); the chat
//! path is appended unless it is already there. No API key is required, and none is ever sent.

use fx_core::{FxError, Message, MessageRole, ModelRequest, ModelResponse, Usage};
use serde::{Deserialize, Serialize};

use crate::{HttpProviderConfig, error_detail, transport_error};

pub(crate) const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:11434";
const CHAT_PATH: &str = "/api/chat";

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    /// The reply is read whole; streaming is never used.
    stream: bool,
    options: WireOptions,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Serialize)]
struct WireOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    /// Ollama's name for the maximum number of tokens generated.
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
}

#[derive(Deserialize)]
struct WireResponse {
    /// Ollama has no response id; the creation timestamp stands in for one.
    #[serde(default)]
    created_at: String,
    message: Option<WireReply>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
}

#[derive(Deserialize)]
struct WireReply {
    #[serde(default)]
    content: Option<String>,
}

fn role(message: &Message) -> &'static str {
    match message.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    }
}

fn chat_url(endpoint: &str) -> String {
    let base = endpoint.trim_end_matches('/');
    if base.ends_with(CHAT_PATH) {
        base.to_string()
    } else {
        format!("{base}{CHAT_PATH}")
    }
}

/// Ollama reports errors as `{"error":"<text>"}`; other servers nest a `message`.
fn ollama_error_detail(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| error_detail(body))
}

pub(crate) async fn complete(
    client: &reqwest::Client,
    config: &HttpProviderConfig,
    request: ModelRequest,
) -> Result<ModelResponse, FxError> {
    if request.messages.is_empty() {
        return Err(FxError::InvalidRequest(
            "ModelRequest must contain at least one message".into(),
        ));
    }

    let wire = WireRequest {
        model: &request.model.0,
        messages: request
            .messages
            .iter()
            .map(|m| WireMessage {
                role: role(m),
                content: &m.content,
            })
            .collect(),
        stream: false,
        options: WireOptions {
            temperature: request.temperature.or(config.temperature),
            num_predict: request.max_tokens.or(config.max_tokens),
        },
    };
    let body = serde_json::to_vec(&wire).map_err(|e| FxError::Serialization(e.to_string()))?;

    // No credentials are ever attached: Ollama needs none, and a key configured for another
    // provider must not travel to this endpoint.
    let response = client
        .post(chat_url(&config.endpoint))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(transport_error)?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(transport_error)?;

    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(FxError::Authentication(format!(
            "endpoint returned {status}"
        )));
    }
    if !status.is_success() {
        return Err(FxError::Provider(format!(
            "endpoint returned {status}: {}",
            ollama_error_detail(&bytes)
        )));
    }

    let parsed: WireResponse = serde_json::from_slice(&bytes)
        .map_err(|_| FxError::InvalidResponse("body is not a valid chat response".into()))?;
    let output = parsed
        .message
        .and_then(|m| m.content)
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| FxError::InvalidResponse("response contains no message content".into()))?;
    // Counts are reported only when Ollama reports them; nothing is estimated.
    let usage = match (parsed.prompt_eval_count, parsed.eval_count) {
        (None, None) => Usage::new(0, 0),
        (prompt, completion) => Usage::new(prompt.unwrap_or(0), completion.unwrap_or(0)),
    };

    Ok(ModelResponse::new(parsed.created_at, output, usage))
}
