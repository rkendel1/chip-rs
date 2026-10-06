//! Generic HTTP model provider for FX.
//!
//! Provider-specific wire formats live in adapter modules; `HttpProvider`
//! selects an adapter by `HttpProviderConfig::provider`.

mod openai_compatible;

use std::fmt;
use std::time::Duration;

use fx_core::{FxError, ModelId, ModelProvider, ModelRequest, ModelResponse, Secret};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Name of the first (and currently only) adapter.
pub const PROVIDER_OPENAI_COMPATIBLE: &str = "openai-compatible";

#[derive(Clone)]
pub struct HttpProviderConfig {
    pub provider: String,
    pub model: ModelId,
    pub endpoint: String,
    pub api_key: Option<Secret>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub timeout: Duration,
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
            temperature: None,
            max_tokens: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_api_key(mut self, api_key: Secret) -> Self {
        self.api_key = Some(api_key);
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
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("timeout", &self.timeout)
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
        if config.provider != PROVIDER_OPENAI_COMPATIBLE {
            return Err(FxError::Configuration(format!(
                "unknown provider '{}' (supported: {PROVIDER_OPENAI_COMPATIBLE})",
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
        openai_compatible::complete(&self.client, &self.config, request).await
    }
}
