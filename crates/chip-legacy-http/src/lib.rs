//! Generic HTTP model provider for FX.
//!
//! Provider-specific wire formats live in adapter modules; `HttpProvider`
//! selects an adapter by `ProviderConfig::provider`. This is the explicit `--executor legacy`
//! compatibility path; Ollama's own API is not supported (use its OpenAI-compatible endpoint).

mod anthropic;
mod openai_compatible;

use std::fmt;

use chip_inference::{
    FxError, ModelProvider, ModelRequest, ModelResponse, PROVIDER_ANTHROPIC,
    PROVIDER_OPENAI_COMPATIBLE, ProviderConfig,
};

pub struct HttpProvider {
    config: ProviderConfig,
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
    pub fn new(config: ProviderConfig) -> Result<Self, FxError> {
        if ![PROVIDER_OPENAI_COMPATIBLE, PROVIDER_ANTHROPIC].contains(&config.provider.as_str()) {
            return Err(FxError::Configuration(format!(
                "unknown provider '{}' (supported: {PROVIDER_OPENAI_COMPATIBLE}, {PROVIDER_ANTHROPIC})",
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
