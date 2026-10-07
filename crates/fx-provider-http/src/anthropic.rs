//! Adapter for the Anthropic Messages API.
//! All JSON structures here are private to this module.

use fx_core::{FxError, Message, MessageRole, ModelRequest, ModelResponse, Usage};
use serde::{Deserialize, Serialize};

use crate::{HttpProviderConfig, error_detail, transport_error};

pub(crate) const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
/// The Messages API requires `max_tokens`; used only when neither the request nor the config sets it.
const FALLBACK_MAX_TOKENS: u32 = 1024;

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<WireMessage<'a>>,
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
    content: Vec<WireBlock>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

fn role(message: &Message) -> &'static str {
    match message.role {
        MessageRole::Assistant => "assistant",
        // System messages are lifted into the top-level `system` field before this is called.
        MessageRole::System | MessageRole::User => "user",
    }
}

pub(crate) async fn complete(
    client: &reqwest::Client,
    config: &HttpProviderConfig,
    request: ModelRequest,
) -> Result<ModelResponse, FxError> {
    let system: Vec<&str> = request
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::System)
        .map(|m| m.content.as_str())
        .collect();
    let messages: Vec<WireMessage<'_>> = request
        .messages
        .iter()
        .filter(|m| m.role != MessageRole::System)
        .map(|m| WireMessage {
            role: role(m),
            content: &m.content,
        })
        .collect();
    if messages.is_empty() {
        return Err(FxError::InvalidRequest(
            "ModelRequest must contain at least one user or assistant message".into(),
        ));
    }

    let wire = WireRequest {
        model: &request.model.0,
        max_tokens: request
            .max_tokens
            .or(config.max_tokens)
            .unwrap_or(FALLBACK_MAX_TOKENS),
        system: (!system.is_empty()).then(|| system.join("\n\n")),
        messages,
        temperature: request.temperature.or(config.temperature),
    };
    let body = serde_json::to_vec(&wire).map_err(|e| FxError::Serialization(e.to_string()))?;

    let mut http = client
        .post(&config.endpoint)
        .header("content-type", "application/json")
        .header("anthropic-version", API_VERSION)
        .body(body);
    if let Some(key) = &config.api_key {
        http = http.header("x-api-key", key.expose());
    }

    if let Some(workspace) = &config.workspace_id {
        http = http.header("anthropic-workspace-id", workspace);
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
        .map_err(|_| FxError::InvalidResponse("body is not a valid messages response".into()))?;
    let output: String = parsed
        .content
        .into_iter()
        .filter(|b| b.kind == "text")
        .filter_map(|b| b.text)
        .collect();
    if output.is_empty() {
        return Err(FxError::InvalidResponse(
            "response contains no text content".into(),
        ));
    }
    let usage = parsed
        .usage
        .map(|u| Usage::new(u.input_tokens, u.output_tokens))
        .unwrap_or_else(|| Usage::new(0, 0));

    Ok(ModelResponse::new(parsed.id, output, usage))
}
