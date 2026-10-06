use std::sync::Arc;

use chip_core::{
    Agent, AgentDecision, CapabilityAvailability, CapabilityDescriptor, CapabilityError,
    CapabilityId, CapabilityProvider, DecisionInput, ExecutionId, ExecutionRequest,
    ScriptedDecision, TestExecutor, Turn,
};
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

/// Declares one deterministic capability for the decision demonstration.
struct DemoCapabilities;

#[async_trait::async_trait]
impl CapabilityProvider for DemoCapabilities {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(vec![CapabilityDescriptor::new(
            CapabilityId::new("test.operation")?,
            "Test Operation",
            "Deterministic test capability",
        )])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
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

    if args.len() > 1 && args[1] == "--test-execution" {
        let agent = Agent::new(Arc::new(TestModelProvider)).with_executor(Arc::new(TestExecutor));
        let request = ExecutionRequest::new(ExecutionId::new("exec-1"), "test operation");
        let result = agent
            .turn_and_execute(Turn::new("Hello"), request)
            .await
            .expect("turn should succeed");

        println!("Chip");
        println!("FX provider: test");
        println!("Turn completed");
        println!("{}", result.turn.response);
        match result.execution.result {
            Ok(execution) => println!("Execution {:?}: {}", execution.status, execution.output),
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-decision" {
        // Deterministic demonstration only: scripted decision, test executor.
        let agent = Agent::new(Arc::new(TestModelProvider))
            .with_decision_boundary(Arc::new(ScriptedDecision::new(
                DecisionInput::RequestCapability {
                    execution_id: ExecutionId::new("decision-1"),
                    capability_id: "test.operation".into(),
                    inputs: Default::default(),
                },
            )))
            .with_capabilities(Arc::new(DemoCapabilities))
            .with_executor(Arc::new(TestExecutor));
        let report = agent
            .decide(Turn::new("Hello"))
            .await
            .expect("turn should succeed");
        println!("Chip");
        println!("Model response: {}", report.turn.response);
        match report.decision {
            Ok(AgentDecision::RequestCapability(request)) => {
                println!("Decision: request capability {}", request.capability_id);
                match agent.execute_capability(&request).await {
                    Ok(execution) => match execution.result {
                        Ok(result) => println!("Execution {:?}: {}", result.status, result.output),
                        Err(error) => {
                            eprintln!("error: {error}");
                            std::process::exit(1);
                        }
                    },
                    Err(error) => {
                        eprintln!("error: {error}");
                        std::process::exit(1);
                    }
                }
            }
            Ok(AgentDecision::Respond(_)) => println!("Decision: respond"),
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-turn" {
        // Deterministic lifecycle demonstration: no network, keys, or Compute.
        let agent = Agent::new(Arc::new(TestModelProvider))
            .with_decision_boundary(Arc::new(ScriptedDecision::new(
                DecisionInput::RequestCapability {
                    execution_id: ExecutionId::new("turn-1"),
                    capability_id: "test.operation".into(),
                    inputs: Default::default(),
                },
            )))
            .with_capabilities(Arc::new(DemoCapabilities))
            .with_executor(Arc::new(TestExecutor));
        match agent.run_turn(Turn::new("Hello")).await {
            Ok(outcome) => {
                println!("Chip");
                println!("Model response: {}", outcome.response.output);
                match &outcome.decision {
                    AgentDecision::RequestCapability(request) => {
                        println!("Decision: request capability {}", request.capability_id)
                    }
                    AgentDecision::Respond(_) => println!("Decision: respond"),
                }
                match &outcome.execution {
                    Some(result) => println!("Execution {:?}: {}", result.status, result.output),
                    None => println!("Execution: none"),
                }
                println!("Events: {}", outcome.events.len());
                println!("Turn completed");
            }
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--compute-test" {
        // Explicit, opt-in: runs one real Compute execution. Never a fallback.
        let agent = Agent::new(Arc::new(TestModelProvider))
            .with_executor(Arc::new(chip_compute::ComputeExecutor::new()));
        let request = ExecutionRequest::new(
            ExecutionId::new("compute-test-1"),
            chip_compute::SELFTEST_INTENT,
        );
        match agent.execute(request).await.result {
            Ok(result) => {
                println!("Chip");
                println!("Compute execution {:?}: {}", result.status, result.output);
                if let Some(receipt) = result.receipt_id {
                    println!("Receipt: {receipt}");
                }
                if result.status != chip_core::ExecutionStatus::Success {
                    std::process::exit(1);
                }
            }
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
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
