//! Generic HTTP model provider for FX.
//!
//! Provider-specific wire formats live in adapter modules; `HttpProvider`
//! selects an adapter by `HttpProviderConfig::provider`.

mod anthropic;
mod ollama;
mod openai_compatible;

use std::fmt;
use std::time::Duration;

use fx_core::{FxError, ModelId, ModelProvider, ModelRequest, ModelResponse, Secret};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Adapter for endpoints speaking the chat-completions wire format.
pub const PROVIDER_OPENAI_COMPATIBLE: &str = "openai-compatible";

/// Adapter for the Anthropic Messages API.
pub const PROVIDER_ANTHROPIC: &str = "anthropic";

/// Adapter for a local Ollama server. Needs no API key.
pub const PROVIDER_OLLAMA: &str = "ollama";

/// The endpoint a provider talks to when none is configured. Only providers with a single
/// well-known endpoint have one; an OpenAI-compatible endpoint is always the caller's choice.
pub fn default_endpoint(provider: &str) -> Option<&'static str> {
    match provider {
        PROVIDER_ANTHROPIC => Some(anthropic::DEFAULT_ENDPOINT),
        PROVIDER_OLLAMA => Some(ollama::DEFAULT_ENDPOINT),
        _ => None,
    }
}

#[derive(Clone)]
pub struct HttpProviderConfig {
    pub provider: String,
    pub model: ModelId,
    pub endpoint: String,
    pub api_key: Option<Secret>,
    /// Sent as `anthropic-workspace-id` by the Anthropic adapter, which needs it for keys that
    /// are not scoped to a workspace. Ignored by other adapters.
    pub workspace_id: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub timeout: Duration,
    /// Ask the endpoint for a JSON object (`"response_format":{"type":"json_object"}`). A request
    /// field, never prompt text, and a request only: the reply is still untrusted output that the
    /// caller must parse strictly. Used by the OpenAI-compatible adapter; ignored by the others.
    pub json_object_output: bool,
    /// Sent as `"chat_template_kwargs":{"enable_thinking":<value>}` when set. Opt-in because some
    /// OpenAI-compatible servers reject fields they do not know. Used by the OpenAI-compatible
    /// adapter; ignored by the others.
    pub enable_thinking: Option<bool>,
}

impl HttpProviderConfig {
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

impl fmt::Debug for HttpProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpProviderConfig")
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

pub struct HttpProvider {
    config: HttpProviderConfig,
    client: reqwest::Client,
}

impl fmt::Debug for HttpProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpProvider")
            .field("config", &self.config)
            .finish()
    }
}

impl HttpProvider {
    pub fn new(config: HttpProviderConfig) -> Result<Self, FxError> {
        if ![
            PROVIDER_OPENAI_COMPATIBLE,
            PROVIDER_ANTHROPIC,
            PROVIDER_OLLAMA,
        ]
        .contains(&config.provider.as_str())
        {
            return Err(FxError::Configuration(format!(
                "unknown provider '{}' (supported: {PROVIDER_OPENAI_COMPATIBLE}, {PROVIDER_ANTHROPIC}, {PROVIDER_OLLAMA})",
                config.provider
            )));
        }
        if config.endpoint.trim().is_empty() {
            return Err(FxError::Configuration("endpoint must not be empty".into()));
        }
        if config.model.0.trim().is_empty() {
            return Err(FxError::Configuration("model must not be empty".into()));
        }
        if config.timeout.is_zero() {
            return Err(FxError::Configuration("timeout must be non-zero".into()));
        }
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|_| FxError::Configuration("failed to build HTTP client".into()))?;
        Ok(Self { config, client })
    }
}

#[async_trait::async_trait]
impl ModelProvider for HttpProvider {
    /// Dropping the returned future cancels the in-flight HTTP request.
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        match self.config.provider.as_str() {
            PROVIDER_ANTHROPIC => anthropic::complete(&self.client, &self.config, request).await,
            PROVIDER_OLLAMA => ollama::complete(&self.client, &self.config, request).await,
            _ => openai_compatible::complete(&self.client, &self.config, request).await,
        }
    }
}

/// Maps reqwest errors without ever including the request (and its headers).
pub(crate) fn transport_error(error: reqwest::Error) -> FxError {
    if error.is_timeout() {
        FxError::Timeout("request exceeded the configured timeout".into())
    } else if error.is_connect() {
        FxError::Http("failed to connect to endpoint".into())
    } else {
        FxError::Http("request failed".into())
    }
}

/// Extracts `error.message` from a provider error body if present.
pub(crate) fn error_detail(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "no error detail".into())
}
