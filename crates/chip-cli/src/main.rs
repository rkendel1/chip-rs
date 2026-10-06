use std::sync::Arc;

use chip_core::{Agent, Turn};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

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

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 && args[1] == "--test" {
        let agent = Agent::new(Arc::new(TestModelProvider));
        let result = agent.turn(Turn::new("Hello")).await.expect("turn should succeed");

        println!("Chip");
        println!("FX provider: test");
        println!("Turn completed");
        println!("{}", result.response);
        return;
    }

    eprintln!("Usage: cargo run -p chip-cli -- --test");
}
