use std::sync::Arc;

use chip_core::{
    Agent, AgentDecision, Capability, CapabilityAvailability, CapabilityDescriptor,
    CapabilityError, CapabilityId, CapabilityProvider, DecisionBoundary, DecisionError,
    DecisionInput, ExecutionId, ExecutionObserver, ExecutionRequest, ExecutionResult,
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

/// Deterministic model for the bounded-cycle demo: it reports whether its request
/// carried an observation.
struct CycleModel;

#[async_trait::async_trait]
impl ModelProvider for CycleModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        let saw_completion = request
            .messages
            .iter()
            .any(|m| m.content.contains("kind: execution.completed"));
        let output = if saw_completion {
            "I was told the execution completed."
        } else {
            "I would like to run the capability."
        };
        Ok(ModelResponse::new(
            "cycle-response",
            output,
            Usage::new(1, 1),
        ))
    }
}

/// First decision requests a capability; the second responds.
struct CycleDecisions(std::sync::atomic::AtomicUsize);

impl DecisionBoundary for CycleDecisions {
    fn decide(
        &self,
        response: &ModelResponse,
        capabilities: &[Capability],
    ) -> Result<AgentDecision, DecisionError> {
        let input = if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            DecisionInput::RequestCapability {
                execution_id: ExecutionId::new("cycle-1"),
                capability_id: "test.operation".into(),
                inputs: Default::default(),
            }
        } else {
            DecisionInput::Respond
        };
        ScriptedDecision::new(input).decide(response, capabilities)
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

    if args.len() > 1 && args[1] == "--test-observation" {
        // Deterministic and offline: a fixed result passed through the observer.
        let agent =
            Agent::new(Arc::new(TestModelProvider)).with_observer(Arc::new(ExecutionObserver));
        let result = ExecutionResult::success(ExecutionId::new("observation-1"), "hello")
            .with_receipt_id("sha256:test-receipt");
        match agent.observe(&result) {
            Ok(observation) => {
                println!("Chip");
                println!("Execution: success");
                println!("Observation: {}", observation.kind.as_str());
                println!("Execution ID: {}", observation.execution_id);
                println!("Output: {}", observation.output.as_deref().unwrap_or(""));
                println!(
                    "Receipt: {}",
                    observation.receipt_id.as_deref().unwrap_or("none")
                );
            }
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-cycle" {
        // Caller-driven bounded cycle; every step below is an explicit call.
        let agent = Agent::new(Arc::new(CycleModel))
            .with_decision_boundary(Arc::new(CycleDecisions(Default::default())))
            .with_capabilities(Arc::new(DemoCapabilities))
            .with_executor(Arc::new(TestExecutor))
            .with_observer(Arc::new(ExecutionObserver));
        println!("Chip bounded cycle (caller-driven, not autonomous)");
        let outcome: Result<(), String> = async {
            let first = agent
                .decide(Turn::new("Run the test operation"))
                .await
                .map_err(|e| e.to_string())?;
            let Ok(AgentDecision::RequestCapability(request)) = first.decision else {
                return Err("turn 1 did not request a capability".into());
            };
            println!("Turn 1: capability requested ({})", request.capability_id);
            let report = agent
                .execute_capability(&request)
                .await
                .map_err(|e| e.to_string())?;
            let result = report.result.map_err(|e| e.to_string())?;
            println!(
                "Execution: {}",
                format!("{:?}", result.status).to_lowercase()
            );
            let observation = agent.observe(&result).map_err(|e| e.to_string())?;
            println!("Observation: {}", observation.kind.as_str());
            let second = agent
                .decide_with_observations(
                    Turn::new("What happened?"),
                    std::slice::from_ref(&observation),
                )
                .await
                .map_err(|e| e.to_string())?;
            match second.decision {
                Ok(AgentDecision::Respond(response)) => {
                    println!("Turn 2: response ({})", response.output)
                }
                Ok(AgentDecision::RequestCapability(_)) => {
                    println!("Turn 2: capability requested (not executed)")
                }
                Err(e) => return Err(e.to_string()),
            }
            Ok(())
        }
        .await;
        match outcome {
            Ok(()) => println!("Cycle completed"),
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
