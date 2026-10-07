//! PR28: a real model escalation inside the autonomous loop.
//!
//! The provider here is the real `HttpProvider` talking to a local mock server, so the whole
//! request/response path is exercised without a network or credentials. No model is called.
//! The one test against an actual provider is opt-in (`CHIP_TEST_REAL_MODEL=1`) and skips
//! otherwise.

#[path = "../../fx-provider-http/tests/common/mod.rs"]
mod common;

use std::process::{Command, Output};
use std::time::Duration;

const SECRET: &str = "sk-pr28-secret-never-printed";

fn completion(content: &str, usage: Option<(u32, u32)>) -> String {
    let mut body = serde_json::json!({
        "id": "resp-pr28",
        "choices": [{"message": {"role": "assistant", "content": content}}],
    });
    if let Some((p, c)) = usage {
        body["usage"] = serde_json::json!({"prompt_tokens": p, "completion_tokens": c});
    }
    body.to_string()
}

const REQUEST: &str = r#"{"decision":"request_capability","capability":"compute.selftest"}"#;

fn run(url: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chip-cli"))
        .arg("--test-real-model-work")
        .args(extra)
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "mock-model")
        .env("CHIP_ENDPOINT", url)
        .env("CHIP_API_KEY", SECRET)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

async fn run_async(url: String, extra: &'static [&'static str]) -> Output {
    tokio::task::spawn_blocking(move || run(&url, extra))
        .await
        .unwrap()
}

fn field(out: &str, name: &str) -> String {
    out.lines()
        .find_map(|l| l.strip_prefix(name))
        .unwrap_or_else(|| panic!("no `{name}` in:\n{out}"))
        .trim()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_decision_drives_one_execution_and_the_loop_finishes_itself() {
    let server = common::start(
        200,
        &completion(REQUEST, Some((120, 17))),
        Duration::from_millis(30),
    )
    .await;
    let out = run_async(server.url.clone(), &["--deterministic-executor"]).await;
    let t = text(&out);
    assert!(out.status.success(), "{t}");

    assert_eq!(field(&t, "Outcome:"), "Completed");
    assert_eq!(field(&t, "Executions:"), "1");
    assert_eq!(field(&t, "Observations:"), "1");
    assert_eq!(field(&t, "Model escalations:"), "1");
    assert_eq!(field(&t, "Model calls:"), "1");
    assert_eq!(field(&t, "Model tokens:"), "137");
    let latency: u64 = field(&t, "Model latency:")
        .trim_end_matches(" ms")
        .parse()
        .unwrap();
    assert!(latency >= 30, "{t}");

    // Exactly one provider request: no retry, repair or fallback call.
    let captured = server.captured.lock().await;
    assert_eq!(captured.len(), 1);

    // The measured context is the context actually sent.
    let body: serde_json::Value = serde_json::from_str(&captured[0].body).unwrap();
    let sent = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap().len())
        .sum::<usize>();
    let measured: usize = field(&t, "Context bytes:").parse().unwrap();
    assert!(measured > 0);
    assert!(
        sent >= measured && sent - measured < 2048,
        "measured {measured} vs sent {sent}"
    );
    assert!(!t.contains(SECRET));
    assert_eq!(
        captured[0].header("authorization"),
        Some(&*format!("Bearer {SECRET}"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unreported_usage_is_not_estimated() {
    let server = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
    let out = run_async(server.url.clone(), &["--deterministic-executor"]).await;
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert_eq!(field(&t, "Model tokens:"), "not reported");
    assert_eq!(field(&t, "Model calls:"), "1");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_failure_still_records_the_call_and_executes_nothing() {
    let server = common::start(401, "{}", Duration::ZERO).await;
    let out = run_async(server.url.clone(), &["--deterministic-executor"]).await;
    let t = text(&out);
    assert!(!out.status.success());
    assert!(t.contains("Failed"), "{t}");
    assert_eq!(field(&t, "Model calls:"), "1");
    assert_eq!(field(&t, "Executions:"), "0");
    assert!(!t.contains(SECRET));
    assert_eq!(server.captured.lock().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_decisions_fail_without_executing_or_retrying() {
    let replies = [
        "I think you should run the self test.",
        r#"{"decision":"delete_everything"}"#,
        r#"{"decision":"request_capability","capability":"shell.exec"}"#,
        r#"{"decision":"request_capability"}"#,
        r#"{"decision":"request_capability","capability":"compute.selftest","extra":1}"#,
        // The model claiming reality is not reality.
        "The operation succeeded.",
        r#"{"decision":"request_capability","capability":"compute.selftest","status":"success","receipt":"r-1"}"#,
        r#"{"schema":"chip.work-decision.v2","decision":"request_capability","capability":"compute.selftest"}"#,
    ];
    for reply in replies {
        let server = common::start(200, &completion(reply, Some((10, 2))), Duration::ZERO).await;
        let out = run_async(server.url.clone(), &["--deterministic-executor"]).await;
        let t = text(&out);
        assert!(!out.status.success(), "{reply}: {t}");
        assert_eq!(field(&t, "Model calls:"), "1", "{reply}");
        assert_eq!(field(&t, "Executions:"), "0", "{reply}");
        assert_eq!(field(&t, "Observations:"), "0", "{reply}");
        assert_eq!(field(&t, "Receipt:"), "none", "{reply}");
        assert!(!t.contains("EvidenceRecorded"), "{reply}");
        assert_eq!(server.captured.lock().await.len(), 1, "{reply}");
    }
}

#[test]
fn an_unconfigured_provider_is_skipped_not_failed() {
    let out = Command::new(env!("CARGO_BIN_EXE_chip-cli"))
        .arg("--test-real-model-work")
        .env_remove("CHIP_PROVIDER")
        .env_remove("CHIP_MODEL")
        .env_remove("CHIP_ENDPOINT")
        .env_remove("CHIP_API_KEY")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(text(&out).contains("SKIPPED: real model provider unavailable"));
}

/// Opt-in: talks to the provider named by `CHIP_*` and the real Compute. Skips unless
/// `CHIP_TEST_REAL_MODEL=1` and the configuration exists; a provider that is configured but
/// fails is a failure, never a skip.
#[test]
fn real_model_escalation() {
    if std::env::var("CHIP_TEST_REAL_MODEL").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set CHIP_TEST_REAL_MODEL=1 to call a real model");
        return;
    }
    let out = Command::new(env!("CARGO_BIN_EXE_chip-cli"))
        .arg("--test-real-model-work")
        .output()
        .unwrap();
    let t = text(&out);
    if out.status.code() == Some(3) {
        assert!(t.contains("SKIPPED"), "{t}");
        eprintln!("SKIPPED: {}", t.trim());
        return;
    }
    assert!(out.status.success(), "{t}");

    assert_eq!(field(&t, "Outcome:"), "Completed");
    assert_eq!(field(&t, "Turns:"), "2");
    assert_eq!(field(&t, "Model calls:"), "1");
    assert_eq!(field(&t, "Model escalations:"), "1");
    assert_eq!(field(&t, "Local decisions:"), "1");
    assert_eq!(field(&t, "Executions:"), "1");
    assert_eq!(field(&t, "Observations:"), "1");
    assert_ne!(field(&t, "Receipt:"), "none", "{t}");
    assert!(field(&t, "Context bytes:").parse::<u64>().unwrap() > 0);
    let tokens = field(&t, "Model tokens:");
    assert!(
        tokens == "not reported" || tokens.parse::<u64>().unwrap() > 0,
        "{t}"
    );
    for label in ["Model latency:", "Compute latency:"] {
        assert!(!field(&t, label).starts_with("0 "), "{label} {t}");
    }

    // The next decision happens inside the loop, after the observation, with no caller turn.
    let at = |needle: &str| {
        t.find(needle)
            .unwrap_or_else(|| panic!("no `{needle}` in:\n{t}"))
    };
    assert!(at("ModelCalled") < at("ExecutionStarted"));
    assert!(at("ExecutionStarted") < at("ObservationRecorded"));
    assert!(at("ObservationRecorded") < at("DecisionStarted (turn 2)"));
    assert!(at("DecisionStarted (turn 2)") < at("LocalDecision: complete"));
    assert!(at("LocalDecision: complete") < at("WorkCompleted"));

    // Nothing sensitive in the output.
    for var in ["CHIP_API_KEY"] {
        if let Ok(secret) = std::env::var(var) {
            assert!(!secret.is_empty() && !t.contains(&secret));
        }
    }
    assert!(!t.contains("Decide the next step"), "prompt leaked");
}
