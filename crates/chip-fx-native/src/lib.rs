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
//! - [`NativeSettings`] is the host's resolved configuration for one explicitly selected
//!   provider (Anthropic, or an OpenAI-compatible endpoint such as OpenAI, OpenRouter or LM
//!   Studio). [`NativeSettings::resolve`] builds it from a [`NativeSelection`] and a
//!   [`CredentialSource`]; credentials are acquired here, in Chip's host layer, never in FX or in the
//!   native provider.
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
//! native transport.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chip_fx::{CancelSignal, FxModelProvider, OptionsPolicy, RequestIds};
use chip_inference::{FxError, ModelProvider, ModelRequest, ModelResponse};
use fx_native_provider::{
    Anthropic, AnthropicConfig, ConfigError, OpenAiCompatible, OpenAiConfig, Secret,
};

mod credentials;
mod port;

pub use credentials::{CredentialSource, EnvCredentials, LAN_HTTP_VARIABLE, NativeSelection};
use port::NativePort;

/// Which wire format the explicitly selected provider speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    /// An OpenAI-style chat-completions endpoint: OpenAI, OpenRouter, LM Studio and others.
    OpenAiCompatible,
    /// The Anthropic Messages API.
    Anthropic,
}

/// The host's resolved settings for one provider. Build it with [`NativeSettings::resolve`]
/// (credentials from a [`CredentialSource`]) or fill it in directly when the host already holds
/// the credential.
#[derive(Clone)]
pub struct NativeSettings {
    /// The wire format.
    pub kind: ProviderKind,
    /// API root, for example `https://api.openai.com/v1`. Plain `http` is accepted only for a
    /// loopback host.
    pub base_url: String,
    /// The credential, or `None` for a keyless local server. It is passed to the provider's
    /// authentication header and nowhere else.
    pub api_key: Option<String>,
    /// Extra request headers, for example OpenRouter's `HTTP-Referer` and `X-Title`. Headers the
    /// adapter owns are refused.
    pub headers: Vec<(String, String)>,
    /// What happens to sampling options FX cannot carry. There is no default.
    pub options: OptionsPolicy,
    /// Per-attempt budget handed to FX. Without one, the native provider's default deadline
    /// applies.
    pub budget: Option<Duration>,
    /// Anthropic only: the `max_tokens` used when a request carries none (the API requires one).
    pub default_max_tokens: Option<u32>,
    /// Ask for one JSON object on every request (OpenAI-compatible endpoints only; Anthropic has
    /// no equivalent and the request is refused before dispatch if this is set).
    pub json_object_output: bool,
    /// OpenAI-compatible endpoints only: send `chat_template_kwargs.enable_thinking` with this
    /// value. Anthropic has no equivalent, so setting it there is a configuration error rather
    /// than a silently dropped request.
    pub template_thinking: Option<bool>,
    /// Allow plain `http` to a private-LAN IP address for a keyless OpenAI-compatible server.
    /// Honored only with no credential and no extra headers; see [`LAN_HTTP_VARIABLE`].
    pub allow_insecure_lan_http: bool,
    /// PEM text of the root certificates to trust in place of the bundled public roots, for an
    /// endpoint behind a TLS-intercepting proxy or signed by a private CA. Certificate validation
    /// stays on. `None` keeps the bundled roots. Applies to both providers.
    pub root_certificates_pem: Option<Vec<u8>>,
    /// An explicit HTTP `CONNECT` proxy for both providers. `None` connects directly; proxy
    /// environment variables are never consulted.
    pub proxy: Option<ProxySettings>,
    /// Seed for the deterministic FX request ids.
    pub id_seed: [u8; 8],
}

impl std::fmt::Debug for NativeSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSettings")
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field(
                "header_names",
                &self.headers.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            )
            .field("options", &self.options)
            .field("budget", &self.budget)
            .field(
                "root_certificates_pem",
                &self.root_certificates_pem.as_ref().map(|pem| pem.len()),
            )
            .field("proxy", &self.proxy)
            .finish_non_exhaustive()
    }
}

/// An explicit HTTP `CONNECT` proxy, chosen by the host. The tunnel carries TLS end to end; for a
/// TLS-intercepting proxy, also trust its CA (`root_certificates_pem`). A proxy that cannot be
/// reached or refuses the tunnel fails the call: there is no direct connection as a fallback.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxySettings {
    /// `http://host:port`, with no credentials in it.
    pub url: String,
    /// Basic credentials for the proxy: username and password. The password is never printed.
    pub credentials: Option<(String, String)>,
    /// Hosts reached directly: `example.com`, `.example.com` or `*.example.com`.
    pub no_proxy: Vec<String>,
}

impl std::fmt::Debug for ProxySettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxySettings")
            .field("url", &self.url)
            .field(
                "username",
                &self.credentials.as_ref().map(|(user, _)| user.as_str()),
            )
            .field("password", &self.credentials.as_ref().map(|_| "[redacted]"))
            .field("no_proxy", &self.no_proxy)
            .finish()
    }
}

/// Applies `proxy` to an FX provider config (both configs have the same proxy methods).
macro_rules! with_proxy {
    ($config:expr, $proxy:expr, $bad:expr) => {{
        let mut config = $config;
        if let Some(proxy) = $proxy {
            config = config.with_proxy(&proxy.url).map_err($bad)?;
            if let Some((username, password)) = &proxy.credentials {
                config = config
                    .with_proxy_credentials(username, Secret::new(password.clone()))
                    .map_err($bad)?;
            }
            for host in &proxy.no_proxy {
                config = config.with_no_proxy(host).map_err($bad)?;
            }
        }
        config
    }};
}

/// Chip's model port over the authoritative FX executor and the native OpenAI-compatible provider.
pub struct NativeModelProvider {
    inner: Arc<FxModelProvider<NativePort>>,
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
        let ids = RequestIds::new(settings.id_seed);
        let bad = |error: ConfigError| FxError::Configuration(error.to_string());
        let key = settings.api_key.map(Secret::new);
        let port = match settings.kind {
            ProviderKind::OpenAiCompatible => {
                let lan_http = settings.allow_insecure_lan_http
                    && credentials::is_plain_http_off_loopback(&settings.base_url);
                if lan_http && key.is_some() {
                    return Err(FxError::Configuration(
                        "a credential is never sent over plain http to a non-loopback host".into(),
                    ));
                }
                let mut config = match key {
                    Some(key) => OpenAiConfig::new(&settings.base_url, key),
                    None if lan_http => {
                        OpenAiConfig::without_key_on_private_lan(&settings.base_url)
                    }
                    None => OpenAiConfig::without_key(&settings.base_url),
                }
                .map_err(bad)?;
                for (name, value) in &settings.headers {
                    config = config.with_header(name, value).map_err(bad)?;
                }
                if let Some(pem) = &settings.root_certificates_pem {
                    config = config.with_root_certificates_pem(pem).map_err(bad)?;
                }
                config = with_proxy!(config, &settings.proxy, bad);
                if let Some(enable) = settings.template_thinking {
                    config = config.with_chat_template_thinking(enable);
                }
                NativePort::OpenAi(OpenAiCompatible::new(config))
            }
            ProviderKind::Anthropic => {
                if settings.template_thinking.is_some() {
                    return Err(FxError::Configuration(
                        "the chat-template thinking switch has no Anthropic equivalent; unset it \
                         (CHIP_ENABLE_THINKING) for the Anthropic provider"
                            .into(),
                    ));
                }
                let key = key.ok_or_else(|| {
                    FxError::Configuration("the Anthropic provider needs an API key".into())
                })?;
                let mut config = AnthropicConfig::new(&settings.base_url, key).map_err(bad)?;
                if let Some(tokens) = settings.default_max_tokens {
                    config = config.with_default_max_tokens(tokens);
                }
                for (name, value) in &settings.headers {
                    config = config.with_header(name, value).map_err(bad)?;
                }
                if let Some(pem) = &settings.root_certificates_pem {
                    config = config.with_root_certificates_pem(pem).map_err(bad)?;
                }
                config = with_proxy!(config, &settings.proxy, bad);
                NativePort::Anthropic(Anthropic::new(config))
            }
        };
        let mut provider = FxModelProvider::new(port, ids, settings.options);
        if settings.json_object_output {
            provider = provider.with_json_object_output();
        }
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
        // A cancellation scoped to this call (`chip_fx::with_call_cancel`) lives on this thread;
        // it is carried to the blocking thread and scoped again there.
        let cancel = chip_fx::current_call_cancel();
        // The FX attempt blocks while it polls the network. Run it where blocking is allowed,
        // and drive the (immediately ready) async wrapper to completion there.
        tokio::task::spawn_blocking(move || match cancel {
            Some(cancel) => {
                handle.block_on(chip_fx::with_call_cancel(cancel, inner.complete(request)))
            }
            None => handle.block_on(inner.complete(request)),
        })
        .await
        .map_err(|error| FxError::Provider(format!("the model call did not finish: {error}")))?
    }
}
