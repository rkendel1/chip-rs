//! Live test against a real endpoint. Skipped unless configured:
//!   FX_PROVIDER_API_KEY, FX_PROVIDER_ENDPOINT, FX_PROVIDER_MODEL
//! (optional FX_PROVIDER_NAME, default "openai-compatible").
//! Run: cargo test -p fx-provider-http --test live_provider -- --nocapture

use fx_core::{Message, MessageRole, ModelProvider, ModelRequest, Secret};
use fx_provider_http::{HttpProvider, HttpProviderConfig, PROVIDER_OPENAI_COMPATIBLE};

#[tokio::test]
async fn live_provider_returns_output() {
    let (Ok(key), Ok(endpoint), Ok(model)) = (
        std::env::var("FX_PROVIDER_API_KEY"),
        std::env::var("FX_PROVIDER_ENDPOINT"),
        std::env::var("FX_PROVIDER_MODEL"),
    ) else {
        eprintln!("skipping live provider test: FX_PROVIDER_* not fully set");
        return;
    };
    let name = std::env::var("FX_PROVIDER_NAME")
        .unwrap_or_else(|_| PROVIDER_OPENAI_COMPATIBLE.to_string());

    let config =
        HttpProviderConfig::new(name, model.clone(), endpoint).with_api_key(Secret::new(key));
    let provider = HttpProvider::new(config).expect("provider should construct");
    let request = ModelRequest::new(
        model,
        vec![Message::new(
            MessageRole::User,
            "Reply with the single word: ok",
        )],
    );

    let response = provider
        .complete(request)
        .await
        .expect("live request should succeed");
    assert!(
        !response.output.trim().is_empty(),
        "response should contain output"
    );
}
