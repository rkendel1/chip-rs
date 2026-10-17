//! First complete path: Turn → Agent → ModelProvider → FX executor → mock HTTP → TurnResult.
//! Lives in chip-cli (the composition root) so chip-core never links the HTTP provider.

#[path = "common/mod.rs"]
mod common;

use std::time::Duration;

use chip_cli::provider_selection::{ExecutorKind, build_model};
use chip_core::{Agent, AgentError, AgentEvent, Turn};
use chip_inference::Secret;
use chip_inference::{PROVIDER_OPENAI_COMPATIBLE, ProviderConfig};

const FAKE_KEY: &str = "fake-key-for-tests";

fn agent(url: &str) -> Agent {
    let config = ProviderConfig::new(PROVIDER_OPENAI_COMPATIBLE, "mock-model", url)
        .with_api_key(Secret::new(FAKE_KEY));
    Agent::with_model(
        build_model(config, ExecutorKind::Fx, &|_| None).unwrap(),
        "mock-model",
    )
}

#[tokio::test]
async fn turn_flows_through_http_provider() {
    let server = common::start(200, common::OK_BODY, Duration::ZERO).await;

    let result = agent(&server.url)
        .turn(Turn::new("Hello Chip"))
        .await
        .unwrap();

    assert_eq!(result.response, "Hello from the mock.");
    assert_eq!(result.events.len(), 5);
    assert!(matches!(
        result.events[1],
        AgentEvent::RequestBuilt { ref model } if model == "mock-model"
    ));
    assert!(!format!("{result:?}").contains(FAKE_KEY));

    let captured = server.captured.lock().await;
    let json: serde_json::Value = serde_json::from_str(&captured[0].body).unwrap();
    assert_eq!(json["model"], "mock-model");
    assert_eq!(json["messages"][0]["content"], "Hello Chip");
}

#[tokio::test]
async fn provider_failure_surfaces_as_agent_error_without_secret() {
    let server = common::start(401, "{}", Duration::ZERO).await;
    let err = agent(&server.url).turn(Turn::new("Hi")).await.unwrap_err();
    // An FX failure keeps its structure. FX reports an HTTP error status as possibly sent (the
    // request reached the provider, which may have acted on it); it never lowers that.
    assert!(matches!(err, AgentError::Inference(_)), "{err:?}");
    assert_eq!(err.delivery(), Some(chip_core::Delivery::PossiblySent));
    assert!(err.to_string().starts_with("provider error: "), "{err}");
    assert!(!format!("{err} {err:?}").contains(FAKE_KEY));
}
