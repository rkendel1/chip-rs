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

/// What the command line chose. Anything left `None` is decided by the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    /// `fx` (the only executor; anything else is refused); see [`ExecutorKind`].
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

/// Which inference executor serves the selected model. There is one: the authoritative FX executor
/// with a native Rust provider (`chip-fx-native`): one attempt per call, structured delivery
/// certainty, no Node process, no retry and no fallback. The former `legacy` executor
/// (`chip-legacy-http`) was removed; asking for it is an error, not a request to fall back.
///
/// `fx` stays accepted as a value of `--executor` / `CHIP_EXECUTOR` so a deployment that set it
/// explicitly keeps working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutorKind {
    /// The authoritative FX executor with a native provider.
    #[default]
    Fx,
}

impl ExecutorKind {
    /// Validates `--executor` and `CHIP_EXECUTOR`. Both are checked, so a valid flag does not hide
    /// a stale environment value. Anything but `fx` fails here, before any request is made.
    pub fn choose(
        flag: Option<&str>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, FxError> {
        for (source, value) in [
            ("--executor", flag.map(str::to_owned)),
            ("CHIP_EXECUTOR", env("CHIP_EXECUTOR")),
        ] {
            let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
                continue;
            };
            match value.trim() {
                "fx" => {}
                "legacy" => {
                    return Err(FxError::Configuration(format!(
                        "{source}=legacy: the legacy executor was removed; FX is the only supported executor \
                         (omit {source}, or use `fx`). Nothing was run"
                    )));
                }
                other => {
                    return Err(FxError::Configuration(format!(
                        "{source}={other:?} is not an executor: FX is the only supported executor (`fx`). \
                         Nothing was run"
                    )));
                }
            }
        }
        Ok(Self::Fx)
    }
}

/// Builds the model provider for `config` on `executor`. With `fx` the provider kind, endpoint,
/// key and options are mapped from the same configuration, and anything the
/// native path cannot carry is a configuration error naming it, never a silent difference.
pub fn build_model(
    config: ProviderConfig,
    executor: ExecutorKind,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Arc<dyn ModelProvider>, FxError> {
    match executor {
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
/// and the LAN opt-in still come from the process environment, as does the CA bundle (public
/// certificates are not a credential).
pub fn build_model_isolated(config: ProviderConfig) -> Result<Arc<dyn ModelProvider>, FxError> {
    let process = |name: &str| std::env::var(name).ok();
    let executor = ExecutorKind::choose(None, &process)?;
    let env = |name: &str| {
        (name == LAN_HTTP_VARIABLE || name == CA_BUNDLE_VARIABLE)
            .then(|| process(name))
            .flatten()
    };
    build_model(config, executor, &env)
}

/// The API root a native provider appends its path to, from the full URL users configure
///  (`.../v1/chat/completions`, `.../v1/messages`).
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
        // Anthropic requires a cap; this is the fallback when nothing sets one.
        selection.default_max_tokens = Some(config.max_tokens.unwrap_or(1024));
        if let Some(workspace) = &config.workspace_id {
            selection
                .headers
                .push(("anthropic-workspace-id".into(), workspace.clone()));
        }
    } else {
        // Anthropic's Messages API has no equivalent.
        selection.json_object_output = config.json_object_output;
    }
    // Carried as the top-level `chat_template_kwargs.enable_thinking` field. On Anthropic, which has
    // no such field, it is refused by name instead of dropped.
    selection.template_thinking = config.enable_thinking;
    selection.allow_insecure_lan_http = lan_http_opt_in(env)?;
    selection.root_certificates_pem = ca_bundle(env)?;
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

/// The environment variable that names a PEM file of root certificates to trust for `https`.
pub const CA_BUNDLE_VARIABLE: &str = "SSL_CERT_FILE";

/// The CA bundle `SSL_CERT_FILE` names, read here because the native provider never touches the
/// filesystem or the environment. Unset or empty keeps the bundled public roots. When it is set it
/// replaces them for every `https` endpoint (as `curl --cacert` does), so a file that cannot be read
/// is an error naming the variable, never a silent fall back to the public roots. A file with no valid
/// certificate is refused when the provider is built.
fn ca_bundle(env: &dyn Fn(&str) -> Option<String>) -> Result<Option<Vec<u8>>, FxError> {
    let Some(path) = env(CA_BUNDLE_VARIABLE)
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
    else {
        return Ok(None);
    };
    std::fs::read(&path).map(Some).map_err(|error| {
        FxError::Configuration(format!(
            "{CA_BUNDLE_VARIABLE}={path} cannot be read ({error}); unset it to use the bundled public \
             roots, or point it at a PEM file of root certificates; nothing was run"
        ))
    })
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
/// users configure (the request's own value wins).
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

    const TEST_ROOT: &str = "-----BEGIN CERTIFICATE-----\nMIIBpTCCAUugAwIBAgIUGxuR7UMf4wCGUFknHqHQLWt5m8UwCgYIKoZIzj0EAwIw\nJzElMCMGA1UEAwwcZngtbmF0aXZlLXByb3ZpZGVyIHRlc3Qgcm9vdDAgFw0yNjEw\nMTYyMzMzMDdaGA8yMTI2MDkyMjIzMzMwN1owJzElMCMGA1UEAwwcZngtbmF0aXZl\nLXByb3ZpZGVyIHRlc3Qgcm9vdDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABD/G\nkpsDC8dks62rbWrLyQOKfPkRE8bJO5zo34h/jpjW6+Bk4E0NxvHG+/Pk5XnE0Kl4\n6ZLRPS23H2e67YN8ey+jUzBRMB0GA1UdDgQWBBSGpTf0iUNc8ZX9GGBfjFmYkX+U\n0DAfBgNVHSMEGDAWgBSGpTf0iUNc8ZX9GGBfjFmYkX+U0DAPBgNVHRMBAf8EBTAD\nAQH/MAoGCCqGSM49BAMCA0gAMEUCIQDnh3EtlQ2kBCfUMrFfqiqIhT/VVZFkqUV+\niZCe/neitgIgb1IFqNXOjw0/7R5oHwzaJiUCaJ+R5Waxy1oxMw8U2LM=\n-----END CERTIFICATE-----\n";

    fn https_config() -> ProviderConfig {
        ProviderConfig::new(
            PROVIDER_OPENAI_COMPATIBLE,
            "m",
            "https://gateway.example/v1/chat/completions",
        )
        .with_api_key(chip_inference::Secret::new("sk-test-key"))
    }

    fn ca_file(tag: &str, text: &str) -> String {
        let path = std::env::temp_dir().join(format!("chip-ca-{}-{tag}.pem", std::process::id()));
        std::fs::write(&path, text).unwrap();
        path.display().to_string()
    }

    fn build(ca: Option<&str>) -> Result<Arc<dyn ModelProvider>, FxError> {
        let pairs: Vec<(&str, &str)> = ca.map(|c| (CA_BUNDLE_VARIABLE, c)).into_iter().collect();
        build_model(https_config(), ExecutorKind::Fx, &env(&pairs))
    }

    #[test]
    fn ssl_cert_file_unset_or_empty_keeps_the_bundled_roots() {
        assert!(build(None).is_ok());
        assert!(build(Some("  ")).is_ok());
    }

    #[test]
    fn a_valid_ca_bundle_named_by_ssl_cert_file_is_accepted() {
        assert!(build(Some(&ca_file("valid", TEST_ROOT))).is_ok());
    }

    #[test]
    fn an_unreadable_ca_bundle_is_an_error_naming_the_variable_never_a_silent_fallback() {
        let Err(FxError::Configuration(why)) = build(Some("/nonexistent/chip-ca.pem")) else {
            panic!("an unreadable bundle must be refused");
        };
        assert!(
            why.contains("SSL_CERT_FILE") && why.contains("nothing was run"),
            "{why}"
        );
        assert!(!why.contains("sk-test-key"));
    }

    #[test]
    fn a_ca_bundle_with_no_certificate_is_refused_not_ignored() {
        for (tag, text) in [("empty", ""), ("garbage", "not a certificate")] {
            assert!(
                matches!(
                    build(Some(&ca_file(tag, text))),
                    Err(FxError::Configuration(_))
                ),
                "{tag}"
            );
        }
    }
}
