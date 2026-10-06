use std::sync::Arc;

use chip_core::{Agent, Turn};
use fx_core::{FxError, Message, MessageRole, ModelProvider, ModelRequest, ModelResponse, Usage};

#[derive(Default)]
struct DemoProvider;

#[async_trait::async_trait]
impl ModelProvider for DemoProvider {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, FxError> {
        Ok(ModelResponse::new(
            "demo-response",
            "Hello from the test provider.",
            Usage::new(4, 6),
        ))
    }
}

#[tokio::main]
async fn main() {
    let provider = Arc::new(DemoProvider);
    let agent = Agent::new(provider);
    let turn = Turn::new("Hello");
    let result = agent.turn(turn).await.expect("turn should complete successfully");

    println!("response: {}", result.response);
    for event in result.events {
        println!("event: {:?}", event);
    }
}
