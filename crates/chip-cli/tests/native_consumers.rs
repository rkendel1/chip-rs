//! Commands that talk to a model but are not `chip work`: they build their provider through the
//! same selection as everything else, so the default executor is FX and `legacy` is only ever
//! explicit. The model is a loopback mock; the `chip` binary is real.

use std::process::{Command, Output};
use std::time::Duration;

#[path = "../../chip-legacy-http/tests/common/mod.rs"]
mod common;

fn run(url: &str, args: &[&str], executor: Option<&str>) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_chip"));
    c.args(args)
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "bench-model")
        .env("CHIP_ENDPOINT", url)
        .env("CHIP_API_KEY", "sk-consumer-secret-never-printed")
        .env_remove("CHIP_EXECUTOR")
        .env_remove("CHIP_ENABLE_THINKING");
    if let Some(executor) = executor {
        c.env("CHIP_EXECUTOR", executor);
    }
    c.output().unwrap()
}

async fn go(
    args: &'static [&'static str],
    executor: Option<&'static str>,
) -> (Output, Vec<String>) {
    let server = common::start(200, common::OK_BODY, Duration::ZERO).await;
    let url = server.url.clone();
    let out = tokio::task::spawn_blocking(move || run(&url, args, executor))
        .await
        .unwrap();
    let bodies = server
        .captured
        .lock()
        .await
        .iter()
        .map(|c| c.body.clone())
        .collect();
    (out, bodies)
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const LIVE: &[&str] = &["--benchmark-live-reasoner", "2"];

#[tokio::test(flavor = "multi_thread")]
async fn the_live_reasoner_benchmark_uses_the_fx_executor_by_default() {
    let (out, bodies) = go(LIVE, None).await;
    assert!(!bodies.is_empty(), "the model was asked: {}", text(&out));
    for body in &bodies {
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["stream"], true, "the fx wire format streams");
    }
    assert!(
        !text(&out).contains("sk-consumer-secret"),
        "the key is never printed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_is_explicit_and_posts_without_streaming() {
    let (out, bodies) = go(LIVE, Some("legacy")).await;
    assert!(!bodies.is_empty(), "{}", text(&out));
    for body in &bodies {
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body.get("stream"), None);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_executor_is_an_error_with_no_request_and_no_fallback() {
    let (out, bodies) = go(LIVE, Some("both")).await;
    assert!(bodies.is_empty(), "nothing was sent: {}", text(&out));
    assert_ne!(out.status.code(), Some(0));
    assert!(text(&out).contains("unknown executor"), "{}", text(&out));
}

const HORIZON: &[&str] = &["--test-horizon-live", "--horizon", "1"];

/// The horizon run needs real Compute before it asks a model, which this environment may not have,
/// so what is checked here is that it builds its provider through the shared selection: an unknown
/// executor and the fx executor's refusal of Ollama's own API both surface before anything is sent.
#[tokio::test(flavor = "multi_thread")]
async fn the_live_horizon_run_builds_its_provider_through_the_shared_selection() {
    let (out, bodies) = go(HORIZON, Some("both")).await;
    assert!(bodies.is_empty(), "{}", text(&out));
    assert!(text(&out).contains("unknown executor"), "{}", text(&out));

    let server = common::start(200, common::OK_BODY, Duration::ZERO).await;
    let url = server.url.clone();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_chip"))
            .args(HORIZON)
            .env("CHIP_PROVIDER", "ollama")
            .env("CHIP_MODEL", "m")
            .env("CHIP_OLLAMA_ENDPOINT", url)
            .env_remove("CHIP_ENDPOINT")
            .env_remove("CHIP_EXECUTOR")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    // fx is the default, and it names the one executor that does serve Ollama's own API.
    assert!(text(&out).contains("--executor legacy"), "{}", text(&out));
    assert_eq!(server.captured.lock().await.len(), 0);
}
