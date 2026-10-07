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

use fx_core::FxError;
use fx_provider_http::HttpProviderConfig;

/// What the command line chose. Anything left `None` is decided by the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
}

/// The configuration this selection resolves to, with `env` as the environment.
///
/// A command-line endpoint wins over both `CHIP_ENDPOINT` and `CHIP_OLLAMA_ENDPOINT`.
pub fn resolve(
    selection: &Selection,
    env: impl Fn(&str) -> Option<String>,
) -> Result<HttpProviderConfig, FxError> {
    crate::config_from_env(|name| match name {
        "CHIP_PROVIDER" => selection.provider.clone().or_else(|| env(name)),
        "CHIP_MODEL" => selection.model.clone().or_else(|| env(name)),
        "CHIP_ENDPOINT" => selection.endpoint.clone().or_else(|| env(name)),
        "CHIP_OLLAMA_ENDPOINT" if selection.endpoint.is_some() => None,
        _ => env(name),
    })
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
        }
    }

    #[test]
    fn the_command_line_wins_over_the_environment() {
        let got = pick(
            sel(
                Some("ollama"),
                Some("qwen3-coder"),
                Some("http://10.0.0.5:11434"),
            ),
            &[
                ("CHIP_PROVIDER", "anthropic"),
                ("CHIP_MODEL", "claude-x"),
                ("CHIP_ENDPOINT", "https://api.example"),
                ("CHIP_OLLAMA_ENDPOINT", "http://env-ollama:1"),
            ],
        )
        .unwrap();
        assert_eq!(
            got,
            (
                "ollama".into(),
                "qwen3-coder".into(),
                "http://10.0.0.5:11434".into()
            )
        );
    }

    #[test]
    fn each_setting_falls_through_independently() {
        // Provider from the command line, model from the environment.
        let got = pick(
            sel(Some("ollama"), None, None),
            &[("CHIP_PROVIDER", "anthropic"), ("CHIP_MODEL", "from-env")],
        )
        .unwrap();
        assert_eq!((got.0.as_str(), got.1.as_str()), ("ollama", "from-env"));
        // Everything from the environment.
        let got = pick(
            Selection::default(),
            &[
                ("CHIP_PROVIDER", "ollama"),
                ("CHIP_MODEL", "m"),
                ("CHIP_ENDPOINT", "http://h:1"),
            ],
        )
        .unwrap();
        assert_eq!(got, ("ollama".into(), "m".into(), "http://h:1".into()));
        // The environment's Ollama-specific endpoint still applies when nothing overrides it.
        let got = pick(
            sel(None, Some("m"), None),
            &[
                ("CHIP_PROVIDER", "ollama"),
                ("CHIP_OLLAMA_ENDPOINT", "http://local:2"),
            ],
        )
        .unwrap();
        assert_eq!(got.2, "http://local:2");
    }

    #[test]
    fn a_provider_default_endpoint_applies_but_a_model_is_never_inferred() {
        let got = pick(sel(Some("ollama"), Some("qwen3-coder"), None), &[]).unwrap();
        assert_eq!(got.2, "http://127.0.0.1:11434");
        let got = pick(
            sel(Some("anthropic"), Some("claude-haiku-4-5-20251001"), None),
            &[],
        )
        .unwrap();
        assert_eq!(got.2, "https://api.anthropic.com/v1/messages");
        // No model anywhere: an explicit failure, for every provider, even one with a default endpoint.
        for provider in ["ollama", "anthropic", "openai-compatible"] {
            let err = pick(sel(Some(provider), None, Some("http://x:1")), &[]).unwrap_err();
            assert!(
                matches!(err, FxError::Configuration(m) if m.contains("CHIP_MODEL")),
                "{provider}"
            );
        }
        // An empty value is no value.
        assert!(pick(sel(Some("ollama"), Some("  "), None), &[]).is_err());
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
}
