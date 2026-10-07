// The work report is one large `json!` literal.
#![recursion_limit = "256"]

//! The Chip command line and the pieces other programs can embed: the work runtime, the runtime
//! service, and the local environment. `main.rs` is only the command dispatcher.

pub mod benchmark;
pub mod corpus_eval;
pub mod decision_state_cmd;
pub mod graph_cmd;
pub mod horizon;
pub mod laya_eval;
pub mod live_benchmark;
pub mod local_environment;
pub mod local_model_bench;
pub mod native;
pub mod pax_work;
pub mod provider_selection;
pub mod service;
pub mod software_work;
pub mod verify;
pub mod wasm_decision_bench;
pub mod work_demo;

use fx_core::{FxError, Secret};
use fx_provider_http::{HttpProviderConfig, PROVIDER_OPENAI_COMPATIBLE};

/// Builds provider config from CHIP_PROVIDER / CHIP_MODEL / CHIP_ENDPOINT / CHIP_API_KEY.
pub fn config_from_env(
    get: impl Fn(&str) -> Option<String>,
) -> Result<HttpProviderConfig, FxError> {
    let required = |name: &str| {
        get(name)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| FxError::Configuration(format!("{name} is not set")))
    };
    let provider = get("CHIP_PROVIDER").unwrap_or_else(|| PROVIDER_OPENAI_COMPATIBLE.to_string());
    // A provider with one well-known endpoint does not need CHIP_ENDPOINT.
    let set = |name: &str| get(name).filter(|v| !v.trim().is_empty());
    let endpoint = match (provider == fx_provider_http::PROVIDER_OLLAMA)
        .then(|| set("CHIP_OLLAMA_ENDPOINT"))
        .flatten()
        .or_else(|| set("CHIP_ENDPOINT"))
    {
        Some(endpoint) => endpoint,
        None => match fx_provider_http::default_endpoint(&provider) {
            Some(endpoint) => endpoint.to_string(),
            None => required("CHIP_ENDPOINT")?,
        },
    };
    let mut config = HttpProviderConfig::new(provider, required("CHIP_MODEL")?, endpoint);
    if let Some(key) = get("CHIP_API_KEY").filter(|k| !k.is_empty()) {
        config = config.with_api_key(Secret::new(key));
    }
    // Opt-in, and explicit either way: anything but `true` or `false` is a configuration error.
    if let Some(value) = set("CHIP_ENABLE_THINKING") {
        match value.trim() {
            "true" => config = config.with_enable_thinking(true),
            "false" => config = config.with_enable_thinking(false),
            _ => {
                return Err(FxError::Configuration(
                    "CHIP_ENABLE_THINKING must be `true` or `false`".into(),
                ));
            }
        }
    }
    if let Some(workspace) = get("CHIP_ANTHROPIC_WORKSPACE_ID").filter(|v| !v.trim().is_empty()) {
        config = config.with_workspace_id(workspace);
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::config_from_env;
    use fx_core::FxError;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn reads_all_fields_and_redacts_key() {
        let config = config_from_env(env(&[
            ("CHIP_PROVIDER", "openai-compatible"),
            ("CHIP_MODEL", "m"),
            ("CHIP_ENDPOINT", "http://localhost/x"),
            ("CHIP_API_KEY", "super-secret-value"),
        ]))
        .unwrap();
        assert_eq!(config.model.0, "m");
        assert_eq!(config.endpoint, "http://localhost/x");
        assert!(config.api_key.is_some());
        assert!(!format!("{config:?}").contains("super-secret-value"));
    }

    #[test]
    fn missing_model_or_endpoint_is_configuration_error() {
        let err = config_from_env(env(&[("CHIP_ENDPOINT", "http://x")])).unwrap_err();
        assert!(matches!(err, FxError::Configuration(_)));
        let err = config_from_env(env(&[("CHIP_MODEL", "m")])).unwrap_err();
        assert!(matches!(err, FxError::Configuration(_)));
    }

    #[test]
    fn thinking_is_off_only_when_explicitly_configured_and_never_guessed() {
        let base = [("CHIP_MODEL", "m"), ("CHIP_ENDPOINT", "http://x")];
        assert_eq!(config_from_env(env(&base)).unwrap().enable_thinking, None);
        for (value, expected) in [("false", false), ("true", true), (" false ", false)] {
            let mut vars = base.to_vec();
            vars.push(("CHIP_ENABLE_THINKING", value));
            assert_eq!(
                config_from_env(env(&vars)).unwrap().enable_thinking,
                Some(expected)
            );
        }
        for bad in ["0", "no", "False", "off"] {
            let mut vars = base.to_vec();
            vars.push(("CHIP_ENABLE_THINKING", bad));
            let err = config_from_env(env(&vars)).unwrap_err();
            assert!(err.to_string().contains("CHIP_ENABLE_THINKING"), "{bad}");
        }
        // The model is never inferred from the endpoint.
        let err = config_from_env(env(&[("CHIP_ENDPOINT", "http://127.0.0.1:8000")])).unwrap_err();
        assert!(err.to_string().contains("CHIP_MODEL"));
    }
}
