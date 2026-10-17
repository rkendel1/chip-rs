//! Where a credential comes from. This is Chip's host concern, not an FX one.
//!
//! ```text
//! CredentialSource  ->  resolved credential  ->  NativeSettings (FX provider configuration)
//! ```
//!
//! Only environment resolution exists. A secure prompt or the Easy-LLM vault can implement
//! [`CredentialSource`] later without any change to provider execution, and neither is a
//! dependency of the runtime. The native FX provider never sees a source: it receives the one
//! resolved key, or none for a local server, through its configuration.
//!
//! Resolution fails before any network request when a provider that needs a key has none.
//! There is no fallback: a named variable that is unset is an error, never a silent switch to
//! another variable or provider.

use std::time::Duration;

use chip_fx::OptionsPolicy;
use chip_inference::FxError;

use crate::{NativeSettings, ProviderKind};

/// A place credentials can be looked up by name. The name is a variable name or key id; what
/// it means is the source's business.
pub trait CredentialSource {
    /// The credential called `name`, or `None` when it is not set (an empty or blank value
    /// counts as not set).
    fn credential(&self, name: &str) -> Option<String>;
}

/// Environment-variable resolution. The lookup is injected so callers and tests control the
/// environment explicitly; [`EnvCredentials::process`] reads the real process environment.
pub struct EnvCredentials<F: Fn(&str) -> Option<String>>(F);

impl<F: Fn(&str) -> Option<String>> EnvCredentials<F> {
    /// Resolves through `lookup`.
    pub fn new(lookup: F) -> Self {
        Self(lookup)
    }
}

fn process_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

impl EnvCredentials<fn(&str) -> Option<String>> {
    /// Resolves from this process's environment.
    pub fn process() -> Self {
        Self(process_var)
    }
}

impl<F: Fn(&str) -> Option<String>> CredentialSource for EnvCredentials<F> {
    fn credential(&self, name: &str) -> Option<String> {
        (self.0)(name)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    }
}

impl ProviderKind {
    fn label(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "OpenAI-compatible",
            Self::Anthropic => "Anthropic",
        }
    }

    /// The API root used when the selection names no endpoint.
    pub fn default_endpoint(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com/v1",
        }
    }

    /// The variable the credential is read from when the selection names none.
    pub fn default_key_env(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
        }
    }
}

/// What the user explicitly chose. Chip already has `--provider`, `--model` and `--endpoint`;
/// this adds nothing that selects for them.
#[derive(Clone, Debug)]
pub struct NativeSelection {
    /// The wire format.
    pub kind: ProviderKind,
    /// API root; the provider's default when `None`.
    pub endpoint: Option<String>,
    /// An alternative variable for the credential (for example `OPENROUTER_API_KEY`).
    pub key_env: Option<String>,
    /// Extra headers, for example OpenRouter's `HTTP-Referer` and `X-Title`.
    pub headers: Vec<(String, String)>,
    /// Per-attempt budget.
    pub budget: Option<Duration>,
    /// Anthropic only: `max_tokens` when a request carries none.
    pub default_max_tokens: Option<u32>,
    /// Ask for one JSON object on every request (OpenAI-compatible endpoints only).
    pub json_object_output: bool,
    /// OpenAI-compatible endpoints only: send `chat_template_kwargs.enable_thinking` with this
    /// value (what `CHIP_ENABLE_THINKING` means). `None` sends nothing.
    pub template_thinking: Option<bool>,
    /// The explicit opt-in (`CHIP_ALLOW_INSECURE_LAN_HTTP=1`) for a plain-`http` endpoint on a
    /// private LAN address, for keyless OpenAI-compatible inference such as LM Studio on another
    /// machine. Off by default, and refused whenever any credential or extra header is present.
    pub allow_insecure_lan_http: bool,
    /// PEM text of the root certificates to trust for `https` instead of the bundled public roots
    /// (an endpoint behind a TLS-intercepting proxy or signed by a private CA). The host reads it,
    /// for example from the file `SSL_CERT_FILE` names; this crate never reads a file or the
    /// environment. `None` keeps the bundled roots. A bundle with no valid certificate is refused
    /// when the provider is built.
    pub root_certificates_pem: Option<Vec<u8>>,
    /// An explicit HTTP `CONNECT` proxy the host chose (for `chip`, from `CHIP_PROXY` and its
    /// companions). `None` connects directly; `HTTP_PROXY` and similar are never read.
    pub proxy: Option<crate::ProxySettings>,
}

impl NativeSelection {
    /// A selection of `kind` with its default endpoint and variable.
    pub fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            endpoint: None,
            key_env: None,
            headers: Vec::new(),
            budget: None,
            default_max_tokens: None,
            json_object_output: false,
            template_thinking: None,
            allow_insecure_lan_http: false,
            root_certificates_pem: None,
            proxy: None,
        }
    }
}

fn is_loopback(endpoint: &str) -> bool {
    let Some(rest) = endpoint.trim().strip_prefix("http://") else {
        return false;
    };
    let host = if rest.starts_with('[') {
        rest.split(']').next().map_or("", |h| &h[1..])
    } else {
        rest.split(['/', ':']).next().unwrap_or_default()
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Plain `http` to anything but loopback: the case the LAN opt-in exists for.
pub(crate) fn is_plain_http_off_loopback(endpoint: &str) -> bool {
    endpoint.trim().starts_with("http://") && !is_loopback(endpoint)
}

/// The environment variable that opts in to the private-LAN plain-`http` exception.
pub const LAN_HTTP_VARIABLE: &str = "CHIP_ALLOW_INSECURE_LAN_HTTP";

fn valid_variable(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
}

impl NativeSettings {
    /// Resolves `selection` against `source` into the configuration for FX's native provider.
    ///
    /// * A provider that needs a key and has none is a [`FxError::Configuration`] naming the
    ///   variable to set, returned before any network request.
    /// * A plain-`http` loopback endpoint (LM Studio, a local server) needs no key; a key is used
    ///   if one is set.
    /// * `options` and `id_seed` are the host's choices, passed through.
    pub fn resolve(
        selection: &NativeSelection,
        source: &dyn CredentialSource,
        options: OptionsPolicy,
        id_seed: [u8; 8],
    ) -> Result<NativeSettings, FxError> {
        let endpoint = selection
            .endpoint
            .clone()
            .unwrap_or_else(|| selection.kind.default_endpoint().to_owned());
        let variable = selection
            .key_env
            .as_deref()
            .unwrap_or_else(|| selection.kind.default_key_env());
        if !valid_variable(variable) {
            return Err(FxError::Configuration(format!(
                "{variable:?} is not a valid environment variable name"
            )));
        }
        let key = source.credential(variable);
        let lan_http = is_plain_http_off_loopback(&endpoint);
        if lan_http {
            // Decided here, from the resolved credential state, before any request is built.
            if !selection.allow_insecure_lan_http {
                return Err(FxError::Configuration(format!(
                    "plain http to a non-loopback host is refused. For a keyless LM Studio-style \
                     server on your own network set {LAN_HTTP_VARIABLE}=1 (a private IP address, \
                     never with a key); otherwise use https (no request was made)"
                )));
            }
            if selection.kind != ProviderKind::OpenAiCompatible {
                return Err(FxError::Configuration(format!(
                    "{LAN_HTTP_VARIABLE} applies to keyless OpenAI-compatible servers only, not \
                     the {} provider (no request was made)",
                    selection.kind.label()
                )));
            }
            if key.is_some() {
                return Err(FxError::Configuration(format!(
                    "refusing to send a credential over plain http to a non-loopback host: \
                     {variable} resolves to a key. Unset it for this keyless server, or use https \
                     ({LAN_HTTP_VARIABLE} never applies to a request that carries a credential; \
                     no request was made)"
                )));
            }
            if !selection.headers.is_empty() {
                return Err(FxError::Configuration(format!(
                    "refusing to add request headers to a plain-http non-loopback endpoint \
                     ({LAN_HTTP_VARIABLE}); no request was made"
                )));
            }
        }
        let keyless_ok = selection.kind == ProviderKind::OpenAiCompatible
            && (is_loopback(&endpoint) || lan_http);
        if key.is_none() && !keyless_ok {
            return Err(FxError::Configuration(format!(
                "the {} provider needs an API key and {variable} is not set; export it, or select \
                 a local endpoint that needs none (no request was made)",
                selection.kind.label()
            )));
        }
        Ok(NativeSettings {
            kind: selection.kind,
            base_url: endpoint,
            api_key: key,
            headers: selection.headers.clone(),
            options,
            budget: selection.budget,
            default_max_tokens: selection.default_max_tokens,
            json_object_output: selection.json_object_output,
            template_thinking: selection.template_thinking,
            allow_insecure_lan_http: selection.allow_insecure_lan_http,
            root_certificates_pem: selection.root_certificates_pem.clone(),
            proxy: selection.proxy.clone(),
            id_seed,
        })
    }
}
