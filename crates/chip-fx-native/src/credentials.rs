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
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

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
        let keyless_ok = selection.kind == ProviderKind::OpenAiCompatible && is_loopback(&endpoint);
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
            id_seed,
        })
    }
}
