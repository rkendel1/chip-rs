//! The configuration that selects one model provider. It describes a selection and its settings; it
//! performs no I/O, and it is not an executor: the model is reached through FX (`chip-fx-native`), the only executor.

use std::fmt;
use std::time::Duration;

use crate::{ModelId, Secret};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// A server speaking the OpenAI chat-completions wire format (OpenAI, OpenRouter, LM Studio, Ollama's
/// `/v1` endpoint, vLLM, and others). The endpoint is always the caller's choice.
pub const PROVIDER_OPENAI_COMPATIBLE: &str = "openai-compatible";

/// The Anthropic Messages API.
pub const PROVIDER_ANTHROPIC: &str = "anthropic";

/// The endpoint a provider talks to when none is configured. Only a provider with a single well-known
/// endpoint has one.
pub fn default_endpoint(provider: &str) -> Option<&'static str> {
    match provider {
        PROVIDER_ANTHROPIC => Some("https://api.anthropic.com/v1/messages"),
        _ => None,
    }
}

#[derive(Clone)]
pub struct ProviderConfig {
    pub provider: String,
    pub model: ModelId,
    pub endpoint: String,
    pub api_key: Option<Secret>,
    /// Sent as `anthropic-workspace-id` for Anthropic keys that are not scoped to a workspace.
    /// Ignored by other providers.
    pub workspace_id: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub timeout: Duration,
    /// Ask the endpoint for a JSON object. A request field, never prompt text, and a request only:
    /// the reply is still untrusted output that the caller must parse strictly. OpenAI-compatible
    /// endpoints only; ignored for Anthropic.
    pub json_object_output: bool,
    /// Sent as `"chat_template_kwargs":{"enable_thinking":<value>}` when set. Opt-in because some
    /// OpenAI-compatible servers reject fields they do not know. Refused for Anthropic by the fx
    /// executor.
    pub enable_thinking: Option<bool>,
}

impl ProviderConfig {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: ModelId::new(model),
            endpoint: endpoint.into(),
            api_key: None,
            workspace_id: None,
            temperature: None,
            max_tokens: None,
            timeout: DEFAULT_TIMEOUT,
            json_object_output: false,
            enable_thinking: None,
        }
    }

    pub fn with_json_object_output(mut self) -> Self {
        self.json_object_output = true;
        self
    }

    pub fn with_enable_thinking(mut self, enable: bool) -> Self {
        self.enable_thinking = Some(enable);
        self
    }

    pub fn with_api_key(mut self, api_key: Secret) -> Self {
        self.api_key = Some(api_key);
        self
    }

    pub fn with_workspace_id(mut self, workspace_id: impl Into<String>) -> Self {
        self.workspace_id = Some(workspace_id.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .field("api_key", &self.api_key)
            .field("workspace_id", &self.workspace_id)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("timeout", &self.timeout)
            .field("json_object_output", &self.json_object_output)
            .field("enable_thinking", &self.enable_thinking)
            .finish()
    }
}
