//! Commands that talk to a model but are not `chip work`: they build their provider through the
//! same selection as everything else, so the executor is FX and nothing else. The model is a loopback mock; the `chip` binary is real.

use std::process::{Command, Output};
use std::time::Duration;

#[path = "common/mod.rs"]
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

/// The removed `legacy` value and any other value are refused before a single request, from the
/// environment, by every model-calling command, and nothing falls back to anything.
#[tokio::test(flavor = "multi_thread")]
async fn the_removed_legacy_executor_and_unknown_values_fail_before_any_request() {
    for value in ["legacy", "both", "FX", "fx,legacy", "0"] {
        let (out, bodies) = go(LIVE, Some(value)).await;
        assert!(
            bodies.is_empty(),
            "{value}: nothing was sent: {}",
            text(&out)
        );
        assert_ne!(out.status.code(), Some(0), "{value}");
        assert!(
            text(&out).contains("FX is the only supported executor"),
            "{value}: {}",
            text(&out)
        );
    }
}

/// `fx` stays accepted, so a deployment that set it explicitly keeps working.
#[tokio::test(flavor = "multi_thread")]
async fn fx_is_still_accepted_as_an_explicit_value() {
    let (out, bodies) = go(LIVE, Some("fx")).await;
    assert!(!bodies.is_empty(), "{}", text(&out));
}

const HORIZON: &[&str] = &["--test-horizon-live", "--horizon", "1"];

/// The horizon run needs real Compute before it asks a model, which this environment may not have,
/// so what is checked here is that it builds its provider through the shared selection: an unknown
/// executor and the refusal of Ollama's own API both surface before anything is sent.
#[tokio::test(flavor = "multi_thread")]
async fn the_live_horizon_run_builds_its_provider_through_the_shared_selection() {
    let (out, bodies) = go(HORIZON, Some("legacy")).await;
    assert!(bodies.is_empty(), "{}", text(&out));
    assert!(
        text(&out).contains("FX is the only supported executor"),
        "{}",
        text(&out)
    );

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
    // Ollama's own API is not supported by any executor; the error names the supported path.
    assert!(text(&out).contains("no longer supported"), "{}", text(&out));
    assert!(text(&out).contains("openai-compatible"), "{}", text(&out));
    assert_eq!(server.captured.lock().await.len(), 0);
}
