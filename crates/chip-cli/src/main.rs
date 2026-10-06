use std::sync::Arc;

use chip_core::{Agent, Turn};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Secret, Usage};
use fx_provider_http::{HttpProvider, HttpProviderConfig, PROVIDER_OPENAI_COMPATIBLE};

#[derive(Default)]
struct TestModelProvider;

#[async_trait::async_trait]
impl ModelProvider for TestModelProvider {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, FxError> {
        Ok(ModelResponse::new(
            "test-response",
            "Hello from the test provider.",
            Usage::new(4, 6),
        ))
    }
}

/// Builds provider config from CHIP_PROVIDER / CHIP_MODEL / CHIP_ENDPOINT / CHIP_API_KEY.
fn config_from_env(get: impl Fn(&str) -> Option<String>) -> Result<HttpProviderConfig, FxError> {
    let required = |name: &str| {
        get(name)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| FxError::Configuration(format!("{name} is not set")))
    };
    let provider = get("CHIP_PROVIDER").unwrap_or_else(|| PROVIDER_OPENAI_COMPATIBLE.to_string());
    let mut config = HttpProviderConfig::new(
        provider,
        required("CHIP_MODEL")?,
        required("CHIP_ENDPOINT")?,
    );
    if let Some(key) = get("CHIP_API_KEY").filter(|k| !k.is_empty()) {
        config = config.with_api_key(Secret::new(key));
    }
    Ok(config)
}

async fn run_configured(prompt: String) -> Result<(), String> {
    let config = config_from_env(|name| std::env::var(name).ok()).map_err(|e| e.to_string())?;
    let provider_name = config.provider.clone();
    let model = config.model.to_string();
    let provider = HttpProvider::new(config).map_err(|e| e.to_string())?;
    let agent = Agent::with_model(Arc::new(provider), model);
    let result = agent
        .turn(Turn::new(prompt))
        .await
        .map_err(|e| e.to_string())?;

    println!("Chip");
    println!("FX provider: {provider_name}");
    println!("Turn completed");
    println!("{}", result.response);
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 && args[1] == "--test" {
        let agent = Agent::new(Arc::new(TestModelProvider));
        let result = agent
            .turn(Turn::new("Hello"))
            .await
            .expect("turn should succeed");

        println!("Chip");
        println!("FX provider: test");
        println!("Turn completed");
        println!("{}", result.response);
        return;
    }

    if args.len() > 1 {
        if let Err(message) = run_configured(args[1..].join(" ")).await {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        return;
    }

    eprintln!("Usage: cargo run -p chip-cli -- --test");
    eprintln!(
        "       CHIP_MODEL=.. CHIP_ENDPOINT=.. [CHIP_PROVIDER=..] [CHIP_API_KEY=..] cargo run -p chip-cli -- \"<prompt>\""
    );
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
}
