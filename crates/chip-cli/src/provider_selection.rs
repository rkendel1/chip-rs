//! Which model Chip uses is an explicit choice, resolved the same way every time:
//!
//! 1. a command-line argument (`--provider`, `--model`, `--endpoint`), then
//! 2. the environment (`CHIP_PROVIDER`, `CHIP_MODEL`, `CHIP_ENDPOINT`, `CHIP_API_KEY`), then
//! 3. the provider's own default (only an endpoint has one), then
//! 4. nothing: a model is always required. It is never inferred from the provider.
//!
//! There is no fallback. If the selected provider or model cannot be used, the run fails and says
//! so; no other provider or model is tried. The selection grants no authority: the provider sits
//! behind FX and the runtime treats every model the same.

use std::sync::Arc;

use async_trait::async_trait;
use chip_fx::OptionsPolicy;
use chip_fx_native::{
    EnvCredentials, LAN_HTTP_VARIABLE, NativeModelProvider, NativeSelection, NativeSettings,
    ProviderKind,
};
use chip_inference::{FxError, ModelProvider, ModelRequest, ModelResponse};
use chip_inference::{PROVIDER_ANTHROPIC, PROVIDER_OPENAI_COMPATIBLE, ProviderConfig};
use chip_legacy_http::HttpProvider;

/// What the command line chose. Anything left `None` is decided by the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    /// `legacy` (the default) or `fx`; see [`ExecutorKind`].
    pub executor: Option<String>,
}

/// The configuration this selection resolves to, with `env` as the environment.
///
/// A command-line endpoint wins over `CHIP_ENDPOINT`.
pub fn resolve(
    selection: &Selection,
    env: impl Fn(&str) -> Option<String>,
) -> Result<ProviderConfig, FxError> {
    crate::config_from_env(|name| match name {
        "CHIP_PROVIDER" => selection.provider.clone().or_else(|| env(name)),
        "CHIP_MODEL" => selection.model.clone().or_else(|| env(name)),
        "CHIP_ENDPOINT" => selection.endpoint.clone().or_else(|| env(name)),
        _ => env(name),
    })
}

/// Which inference executor serves the selected model. The choice is explicit; nothing is tried
/// as a fallback for the other.
///
/// * `fx` (the default): the authoritative FX executor with a native Rust provider
///   (`chip-fx-native`). One attempt per call, structured delivery certainty, no Node process.
/// * `legacy`: `chip-legacy-http`, the original HTTP adapters for OpenAI-compatible endpoints and
///   Anthropic. Explicit only, and on its way out; Ollama's own API is not served by either executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutorKind {
    /// `chip-legacy-http`. Kept as an explicit choice while its remaining consumers move.
    Legacy,
    /// The authoritative FX executor with a native provider.
    #[default]
    Fx,
}

impl ExecutorKind {
    /// The executor named by `--executor`, else `CHIP_EXECUTOR`, else the default.
    pub fn choose(
        flag: Option<&str>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, FxError> {
        let named = flag
            .map(str::to_owned)
            .or_else(|| env("CHIP_EXECUTOR"))
            .filter(|v| !v.trim().is_empty());
        match named.as_deref().map(str::trim) {
            None => Ok(Self::default()),
            Some("legacy") => Ok(Self::Legacy),
            Some("fx") => Ok(Self::Fx),
            Some(other) => Err(FxError::Configuration(format!(
                "unknown executor {other:?}: use `legacy` or `fx`"
            ))),
        }
    }
}

/// Builds the model provider for `config` on `executor`. With `fx` the provider kind, endpoint,
/// key and options are mapped from the same configuration the legacy path reads, and anything the
/// native path cannot carry is a configuration error naming it, never a silent difference.
pub fn build_model(
    config: ProviderConfig,
    executor: ExecutorKind,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Arc<dyn ModelProvider>, FxError> {
    match executor {
        ExecutorKind::Legacy => Ok(Arc::new(HttpProvider::new(config)?)),
        ExecutorKind::Fx => fx_model(config, env),
    }
}

/// The model provider for a command that has no `--executor` flag: the executor comes from
/// `CHIP_EXECUTOR` (default `fx`) and keys from the process environment. There is no fallback
/// between executors.
pub fn build_model_from_process(config: ProviderConfig) -> Result<Arc<dyn ModelProvider>, FxError> {
    let env = |name: &str| std::env::var(name).ok();
    let executor = ExecutorKind::choose(None, &env)?;
    build_model(config, executor, &env)
}

/// Like [`build_model_from_process`], for a secondary model (the micro shadow) that must never use
/// the work model's credentials: the provider's own key variables (`ANTHROPIC_API_KEY`,
/// `OPENAI_API_KEY`) are hidden, so the key is only what the caller put in `config`. The executor
/// and the LAN opt-in still come from the process environment.
pub fn build_model_isolated(config: ProviderConfig) -> Result<Arc<dyn ModelProvider>, FxError> {
    let process = |name: &str| std::env::var(name).ok();
    let executor = ExecutorKind::choose(None, &process)?;
    let env = |name: &str| (name == LAN_HTTP_VARIABLE).then(|| process(name)).flatten();
    build_model(config, executor, &env)
}

/// The API root a native provider appends its path to, from the full URL the legacy adapters post
/// to (`.../v1/chat/completions`, `.../v1/messages`).
fn api_root(endpoint: &str, kind: ProviderKind) -> Result<String, FxError> {
    let suffix = match kind {
        ProviderKind::OpenAiCompatible => "/chat/completions",
        ProviderKind::Anthropic => "/messages",
    };
    endpoint
        .trim()
        .trim_end_matches('/')
        .strip_suffix(suffix)
        .map(str::to_owned)
        .ok_or_else(|| {
            FxError::Configuration(format!(
                "the fx executor needs an endpoint ending in {suffix} (got {}); nothing was run",
                endpoint_identity(endpoint)
            ))
        })
}

fn fx_model(
    config: ProviderConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Arc<dyn ModelProvider>, FxError> {
    let kind = match config.provider.as_str() {
        PROVIDER_OPENAI_COMPATIBLE => ProviderKind::OpenAiCompatible,
        PROVIDER_ANTHROPIC => ProviderKind::Anthropic,
        other => {
            return Err(FxError::Configuration(format!(
                "the {other} provider is not supported (supported: {PROVIDER_OPENAI_COMPATIBLE}, \
                 {PROVIDER_ANTHROPIC}); Ollama is reached through its OpenAI-compatible endpoint"
            )));
        }
    };
    let mut selection = NativeSelection::new(kind);
    selection.endpoint = Some(api_root(&config.endpoint, kind)?);
    selection.budget = Some(config.timeout);
    if kind == ProviderKind::Anthropic {
        // The same fallback the legacy Anthropic adapter uses when nothing sets a cap.
        selection.default_max_tokens = Some(config.max_tokens.unwrap_or(1024));
        if let Some(workspace) = &config.workspace_id {
            selection
                .headers
                .push(("anthropic-workspace-id".into(), workspace.clone()));
        }
    } else {
        // Anthropic's Messages API has no equivalent; the legacy adapter ignores the flag there too.
        selection.json_object_output = config.json_object_output;
    }
    // Carried as the same `chat_template_kwargs.enable_thinking` field the legacy adapter sends. On
    // Anthropic the legacy adapter ignored it silently; the fx executor refuses it by name instead.
    selection.template_thinking = config.enable_thinking;
    selection.allow_insecure_lan_http = lan_http_opt_in(env)?;
    // The key Chip already resolved from CHIP_API_KEY wins; otherwise the provider's own variable.
    let chip_key = config.api_key.as_ref().map(|k| k.expose().to_string());
    if chip_key.is_some() {
        selection.key_env = Some("CHIP_API_KEY".into());
    }
    let source = EnvCredentials::new(|name| {
        if name == "CHIP_API_KEY" {
            chip_key.clone()
        } else {
            env(name)
        }
    });
    let settings =
        NativeSettings::resolve(&selection, &source, OptionsPolicy::Carry, *b"chip-cli")?;
    let inner = NativeModelProvider::new(settings)?;
    Ok(Arc::new(ConfigDefaults {
        inner,
        max_tokens: config.max_tokens,
        temperature: config.temperature,
    }))
}

/// `CHIP_ALLOW_INSECURE_LAN_HTTP`: `1` opts in, unset or `0` does not, anything else is an error
/// (an ambiguous security switch must not be guessed at).
fn lan_http_opt_in(env: &dyn Fn(&str) -> Option<String>) -> Result<bool, FxError> {
    match env(LAN_HTTP_VARIABLE).as_deref().map(str::trim) {
        None | Some("") | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(_) => Err(FxError::Configuration(format!(
            "{LAN_HTTP_VARIABLE} must be `1` or `0`"
        ))),
    }
}

/// Applies the configured `max_tokens` and `temperature` to a request that carries none, as the
/// legacy adapters do (the request's own value wins).
struct ConfigDefaults {
    inner: NativeModelProvider,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
}

#[async_trait]
impl ModelProvider for ConfigDefaults {
    async fn complete(&self, mut request: ModelRequest) -> Result<ModelResponse, FxError> {
        request.max_tokens = request.max_tokens.or(self.max_tokens);
        request.temperature = request.temperature.or(self.temperature);
        self.inner.complete(request).await
    }
}

/// An endpoint with nothing in it that could be a credential: scheme, host and port only. Userinfo,
/// path and query (where a token could sit) are dropped.
pub fn endpoint_identity(endpoint: &str) -> String {
    let (scheme, rest) = endpoint.split_once("://").unwrap_or(("", endpoint));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if scheme.is_empty() {
        host.to_string()
    } else {
        format!("{scheme}://{host}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    fn pick(
        selection: Selection,
        pairs: &[(&str, &str)],
    ) -> Result<(String, String, String), FxError> {
        resolve(&selection, env(pairs)).map(|c| (c.provider, c.model.to_string(), c.endpoint))
    }

    fn sel(provider: Option<&str>, model: Option<&str>, endpoint: Option<&str>) -> Selection {
        Selection {
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
            endpoint: endpoint.map(str::to_string),
            executor: None,
        }
    }

    #[test]
    fn the_command_line_wins_over_the_environment() {
        let got = pick(
            sel(
                Some("openai-compatible"),
                Some("qwen3-coder"),
                Some("http://10.0.0.5:11434/v1/chat/completions"),
            ),
            &[
                ("CHIP_PROVIDER", "anthropic"),
                ("CHIP_MODEL", "claude-x"),
                ("CHIP_ENDPOINT", "https://api.example"),
            ],
        )
        .unwrap();
        assert_eq!(
            got,
            (
                "openai-compatible".into(),
                "qwen3-coder".into(),
                "http://10.0.0.5:11434/v1/chat/completions".into()
            )
        );
    }

    #[test]
    fn each_setting_falls_through_independently() {
        // Provider from the command line, model from the environment.
        let got = pick(
            sel(Some("anthropic"), None, None),
            &[
                ("CHIP_PROVIDER", "openai-compatible"),
                ("CHIP_MODEL", "from-env"),
            ],
        )
        .unwrap();
        assert_eq!((got.0.as_str(), got.1.as_str()), ("anthropic", "from-env"));
        // Everything from the environment.
        let got = pick(
            Selection::default(),
            &[
                ("CHIP_PROVIDER", "openai-compatible"),
                ("CHIP_MODEL", "m"),
                ("CHIP_ENDPOINT", "http://h:1"),
            ],
        )
        .unwrap();
        assert_eq!(
            got,
            ("openai-compatible".into(), "m".into(), "http://h:1".into())
        );
    }

    #[test]
    fn a_provider_default_endpoint_applies_but_a_model_is_never_inferred() {
        let got = pick(
            sel(Some("anthropic"), Some("claude-haiku-4-5-20251001"), None),
            &[],
        )
        .unwrap();
        assert_eq!(got.2, "https://api.anthropic.com/v1/messages");
        // No model anywhere: an explicit failure, for every provider, even one with a default endpoint.
        for provider in ["anthropic", "openai-compatible"] {
            let err = pick(sel(Some(provider), None, Some("http://x:1")), &[]).unwrap_err();
            assert!(
                matches!(err, FxError::Configuration(m) if m.contains("CHIP_MODEL")),
                "{provider}"
            );
        }
        // An empty value is no value.
        assert!(pick(sel(Some("anthropic"), Some("  "), None), &[]).is_err());
    }

    #[test]
    fn no_endpoint_for_a_provider_without_a_default_is_an_error() {
        let err = pick(sel(Some("openai-compatible"), Some("m"), None), &[]).unwrap_err();
        assert!(matches!(err, FxError::Configuration(m) if m.contains("CHIP_ENDPOINT")));
    }

    #[test]
    fn an_endpoint_identity_holds_no_credential() {
        for (given, identity) in [
            ("http://127.0.0.1:11434", "http://127.0.0.1:11434"),
            ("http://127.0.0.1:11434/api/chat", "http://127.0.0.1:11434"),
            (
                "https://user:hunter2@host.example:8443/v1/x?key=sk-secret#frag",
                "https://host.example:8443",
            ),
            (
                "https://api.anthropic.com/v1/messages",
                "https://api.anthropic.com",
            ),
            ("localhost:11434", "localhost:11434"),
        ] {
            let got = endpoint_identity(given);
            assert_eq!(got, identity, "{given}");
            assert!(
                !got.contains("hunter2") && !got.contains("sk-secret") && !got.contains("key=")
            );
        }
    }

    #[test]
    fn the_api_key_never_appears_in_the_resolved_configuration_text() {
        let c = resolve(
            &sel(Some("anthropic"), Some("m"), None),
            env(&[("CHIP_API_KEY", "sk-ant-very-secret")]),
        )
        .unwrap();
        assert!(!format!("{c:?}").contains("sk-ant-very-secret"));
    }

    #[test]
    fn ollamas_own_api_is_refused_with_the_supported_path() {
        let err = pick(sel(Some("ollama"), Some("m"), Some("http://h:1")), &[]).unwrap_err();
        let FxError::Configuration(message) = err else {
            panic!("not a configuration error")
        };
        assert!(
            message.contains("openai-compatible") && message.contains("11434/v1/chat/completions"),
            "{message}"
        );
    }
}
