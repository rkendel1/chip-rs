//! Adapter for endpoints speaking the chat-completions wire format.
//! All JSON structures here are private to this module.

use fx_core::{FxError, Message, MessageRole, ModelRequest, ModelResponse, Usage};
use serde::{Deserialize, Serialize};

use crate::HttpProviderConfig;

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    id: String,
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireReply,
}

#[derive(Deserialize)]
struct WireReply {
    content: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

fn role(message: &Message) -> &'static str {
    match message.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    }
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
        max_tokens: request.max_tokens.or(config.max_tokens),
        temperature: request.temperature.or(config.temperature),
    };
    let body = serde_json::to_vec(&wire).map_err(|e| FxError::Serialization(e.to_string()))?;

    let mut http = client
        .post(&config.endpoint)
        .header("content-type", "application/json")
        .body(body);
    if let Some(key) = &config.api_key {
        http = http.bearer_auth(key.expose());
    }

    let response = http.send().await.map_err(transport_error)?;
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
            error_detail(&bytes)
        )));
    }

    let parsed: WireResponse = serde_json::from_slice(&bytes)
        .map_err(|_| FxError::InvalidResponse("body is not a valid completion response".into()))?;
    let output = parsed
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .ok_or_else(|| FxError::InvalidResponse("response contains no message content".into()))?;
    let usage = parsed
        .usage
        .map(|u| Usage::new(u.prompt_tokens, u.completion_tokens))
        .unwrap_or_else(|| Usage::new(0, 0));

    Ok(ModelResponse::new(parsed.id, output, usage))
}

/// Maps reqwest errors without ever including the request (and its headers).
fn transport_error(error: reqwest::Error) -> FxError {
    if error.is_timeout() {
        FxError::Timeout("request exceeded the configured timeout".into())
    } else if error.is_connect() {
        FxError::Http("failed to connect to endpoint".into())
    } else {
        FxError::Http("request failed".into())
    }
}

/// Extracts `error.message` from a provider error body if present.
fn error_detail(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "no error detail".into())
}
