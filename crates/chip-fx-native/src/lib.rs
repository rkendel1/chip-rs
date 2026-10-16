//! # chip-fx-native
//!
//! Chip's model port served by the authoritative FX executor with a **native Rust provider**
//! (`fx-native-provider` from `rkendel1/fx`). No Node process, no JavaScript runtime and no
//! second inference executor: the request goes through [`chip_fx::FxModelProvider`], which drives
//! `fx_core::execute` over `fx_single_attempt`, and the only provider-specific code is the
//! `ProviderPort` the host chooses here.
//!
//! This crate is a thin composition and holds no policy:
//!
//! - [`NativeSettings`] is the host's explicit configuration: an OpenAI-compatible base URL, an
//!   API key it passes in (this crate reads no environment and no file), and an optional
//!   per-attempt budget.
//! - [`NativeModelProvider`] is the [`chip_inference::ModelProvider`]. One `complete` call is
//!   one FX attempt: no retry, no fallback, no model routing, no second provider call.
//!
//! The blocking network call runs on the runtime's blocking pool, so an async caller (Chip's
//! work loop) is not stalled while a model call is in flight.
//!
//! What it supports is what `fx-native-provider` supports: text conversations only. Sampling
//! options FX cannot carry follow the [`OptionsPolicy`] the host chose. Tool definitions,
//! images and structured output never reach the wire. See `docs/product/native-provider.md`
//! and `docs/rebuild/NATIVE_PROVIDER.md` in `rkendel1/fx`.
//!
//! `chip-fx` stays free of HTTP and still compiles for `wasm32`; only this crate links the
//! native transport. `chip-legacy-http` is untouched.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chip_fx::{CancelSignal, FxModelProvider, OptionsPolicy, RequestIds};
use chip_inference::{FxError, ModelProvider, ModelRequest, ModelResponse};
use fx_native_provider::{OpenAiCompatible, OpenAiConfig, Secret};

/// The host's explicit settings for one OpenAI-compatible endpoint.
#[derive(Clone)]
pub struct NativeSettings {
    /// API root, for example `https://api.openai.com/v1`. Plain `http` is accepted only for a
    /// loopback host.
    pub base_url: String,
    /// The credential. It is passed to the provider's authorization header and nowhere else.
    pub api_key: String,
    /// What happens to sampling options FX cannot carry. There is no default.
    pub options: OptionsPolicy,
    /// Per-attempt budget handed to FX. Without one, the native provider's default deadline
    /// applies.
    pub budget: Option<Duration>,
    /// Seed for the deterministic FX request ids.
    pub id_seed: [u8; 8],
}

impl std::fmt::Debug for NativeSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSettings")
            .field("base_url", &self.base_url)
            .field("api_key", &"[redacted]")
            .field("options", &self.options)
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

/// Chip's model port over the authoritative FX executor and the native OpenAI-compatible provider.
pub struct NativeModelProvider {
    inner: Arc<FxModelProvider<OpenAiCompatible>>,
}

impl NativeModelProvider {
    /// Builds the provider. A configuration the native provider refuses (cleartext remote host,
    /// credentials in the URL, empty key) is a [`FxError::Configuration`] and no connection is made.
    pub fn new(settings: NativeSettings) -> Result<Self, FxError> {
        Self::build(settings, None)
    }

    /// As [`NativeModelProvider::new`], with a cancellation signal FX polls between provider polls.
    pub fn with_cancel(
        settings: NativeSettings,
        cancel: Arc<dyn CancelSignal>,
    ) -> Result<Self, FxError> {
        Self::build(settings, Some(cancel))
    }

    fn build(
        settings: NativeSettings,
        cancel: Option<Arc<dyn CancelSignal>>,
    ) -> Result<Self, FxError> {
        let config = OpenAiConfig::new(&settings.base_url, Secret::new(settings.api_key))
            .map_err(|error| FxError::Configuration(error.to_string()))?;
        let mut provider = FxModelProvider::new(
            OpenAiCompatible::new(config),
            RequestIds::new(settings.id_seed),
            settings.options,
        );
        if let Some(budget) = settings.budget {
            provider = provider.with_budget(budget);
        }
        if let Some(cancel) = cancel {
            provider = provider.with_cancel(cancel);
        }
        Ok(Self {
            inner: Arc::new(provider),
        })
    }

    /// Counters for audit: attempts started and options dropped.
    pub fn stats(&self) -> chip_fx::ProviderStats {
        self.inner.stats()
    }
}

#[async_trait]
impl ModelProvider for NativeModelProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        let inner = Arc::clone(&self.inner);
        let handle = tokio::runtime::Handle::current();
        // The FX attempt blocks while it polls the network. Run it where blocking is allowed,
        // and drive the (immediately ready) async wrapper to completion there.
        tokio::task::spawn_blocking(move || handle.block_on(inner.complete(request)))
            .await
            .map_err(|error| FxError::Provider(format!("the model call did not finish: {error}")))?
    }
}
