//! PR41/PR43: the `--test-pax-work` CLI. The provider is a mock HTTP server (the real `HttpProvider`
//! talks to it); PAX, Cargo and the project are real. Nothing replaces the PAX process. If PAX is
//! not installed the tests say SKIPPED and return. The one test against a real model is opt-in
//! (`CHIP_TEST_REAL_MODEL=1`).

#[path = "../../fx-provider-http/tests/common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

fn completion(content: &str) -> String {
    serde_json::json!({
        "id": "resp-pax",
        "choices": [{"message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 4},
    })
    .to_string()
}

fn unique(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-cli-pax-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn project(tag: &str, test_body: &str) -> PathBuf {
    let dir = unique(tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"clifixture_{tag}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        format!("#[cfg(test)]\nmod tests {{\n    #[test]\n    fn the_test() {{\n        {test_body}\n    }}\n}}\n"),
    )
    .unwrap();
    dir
}

const REQUEST: &str =
    r#"{"schema":"chip.work-decision.v1","decision":"request_capability","capability":"pax.test"}"#;

fn run(url: &str, workdir: &Path, extra: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chip-cli"));
    command
        .arg("--test-pax-work")
        .arg("--workdir")
        .arg(workdir)
        .args(extra)
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "mock-model")
        .env("CHIP_ENDPOINT", url)
        .env("CHIP_API_KEY", "sk-pax-secret-never-printed")
        .env_remove("PAX_BIN");
    for (k, v) in envs {
        command.env(k, v);
    }
    command.output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn field(out: &str, name: &str) -> String {
    out.lines()
        .find_map(|l| l.trim_start().strip_prefix(name))
        .unwrap_or_else(|| panic!("no `{name}` in:\n{out}"))
        .trim()
        .to_string()
}

async fn go(reply: &str, dir: &Path, envs: &[(&str, &str)]) -> Option<(Output, String, usize)> {
    let server = common::start(200, &completion(reply), Duration::ZERO).await;
    let (url, d, e): (String, PathBuf, Vec<(String, String)>) = (
        server.url.clone(),
        dir.to_path_buf(),
        envs.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    );
    let out = tokio::task::spawn_blocking(move || {
        let pairs: Vec<(&str, &str)> = e.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        run(&url, &d, &[], &pairs)
    })
    .await
    .unwrap();
    let t = text(&out);
    if out.status.code() == Some(3) && t.contains("PAX unavailable") && envs.is_empty() {
        eprintln!("SKIPPED: {}", t.trim());
        return None;
    }
    let calls = server.captured.lock().await.len();
    Some((out, t, calls))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_passing_project_completes_because_pax_said_passed() {
    let dir = project("pass", "assert_eq!(2 + 2, 4);");
    let Some((out, t, calls)) = go(REQUEST, &dir, &[]).await else {
        return;
    };
    assert!(out.status.success(), "{t}");
    assert_eq!(calls, 1, "one model call, no retry");
    assert_eq!(field(&t, "Executions:"), "1");
    assert_eq!(field(&t, "Observations:"), "1");
    assert_eq!(field(&t, "Evidence records:"), "1");
    assert_eq!(field(&t, "Receipt:"), "none (PAX issues no receipt)");
    // The exact invocation Chip built, and nothing from the model in it.
    let invocation = field(&t, "Invocation:");
    assert!(
        invocation.ends_with(&format!("--dir {} --json test", dir.display())),
        "{invocation}"
    );
    assert_eq!(field(&t, "PAX status:"), "passed");
    assert_eq!(field(&t, "PAX reason:"), "tests-passed");
    assert_eq!(field(&t, "PAX tool:"), "cargo");
    assert_eq!(field(&t, "Native exit code:"), "0");
    assert_eq!(field(&t, "Goal satisfied:"), "true");
    assert_eq!(field(&t, "Terminal:"), "Completed");
    assert!(field(&t, "Verified outputs:").starts_with("1 of 1"), "{t}");
    assert!(dir.join("target").exists(), "Cargo really ran");
    assert!(t.contains("unauthorized_executions=0 unauthorized_completions=0 false_completions=0 evidence_without_observation=0 observation_without_execution=0 execution_without_valid_request=0 limit_violations=0"), "{t}");
    assert!(!t.contains("sk-pax-secret"), "no credential in the output");
    // Order: model, validation, process, observation, evidence, evaluation, completion.
    let at = |needle: &str| {
        t.find(needle)
            .unwrap_or_else(|| panic!("no `{needle}` in:\n{t}"))
    };
    assert!(at("ModelCalled") < at("ExecutionStarted"));
    assert!(at("ExecutionStarted") < at("ObservationRecorded"));
    assert!(at("ObservationRecorded") < at("EvidenceRecorded"));
    assert!(at("EvidenceRecorded") < at("GoalEvaluated:satisfied"));
    assert!(at("GoalEvaluated:satisfied") < at("WorkCompleted"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_project_is_failed_and_not_completed() {
    let dir = project("fail", "assert_eq!(2 + 2, 5);");
    let Some((out, t, _)) = go(REQUEST, &dir, &[]).await else {
        return;
    };
    // The chain happened and the audit is clean: reporting what PAX established is the product.
    assert!(out.status.success(), "{t}");
    assert_eq!(field(&t, "Executions:"), "1");
    assert_eq!(field(&t, "PAX status:"), "failed");
    assert_eq!(field(&t, "PAX reason:"), "tests-failed");
    assert_eq!(field(&t, "Native exit code:"), "101");
    assert_eq!(field(&t, "Goal satisfied:"), "false");
    assert!(field(&t, "Terminal:").starts_with("Blocked"), "{t}");
    assert!(field(&t, "Verified outputs:").starts_with("0 of 1"), "{t}");
    assert!(!t.contains("WorkCompleted"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_with_no_tests_is_not_run_and_is_not_completed() {
    // Exit code 0, and PAX says no tests executed: the exit code must not be read as success.
    let dir = unique("zero");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"clifixture_zero\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let Some((out, t, _)) = go(REQUEST, &dir, &[]).await else {
        return;
    };
    assert!(out.status.success(), "{t}");
    assert_eq!(field(&t, "PAX status:"), "not_run");
    assert_eq!(field(&t, "PAX reason:"), "no-tests-executed");
    assert_eq!(field(&t, "Native exit code:"), "0");
    assert_eq!(field(&t, "Goal satisfied:"), "false");
    assert!(field(&t, "Terminal:").starts_with("Blocked"), "{t}");
    assert!(field(&t, "Verified outputs:").starts_with("0 of 1"), "{t}");
    assert!(!t.contains("WorkCompleted"));
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_invocations_never_reach_pax() {
    for reply in [
        r#"{"decision":"request_capability","capability":"pax.test","inputs":{}}"#,
        r#"{"decision":"request_capability","capability":"pax.test","inputs":{"command":"pax test && echo pwned"}}"#,
        r#"{"decision":"request_capability","capability":"pax.test","executable":"/bin/sh"}"#,
        r#"{"decision":"request_capability","capability":"pax.test","cwd":"/"}"#,
        r#"{"decision":"request_capability","capability":"pax.test","status":"passed"}"#,
        r#"{"decision":"request_capability","capability":"pax.test","exit_code":0}"#,
        r#"{"decision":"request_capability","capability":"pax.test","result":"passed"}"#,
        r#"{"decision":"request_capability","capability":"pax.build"}"#,
        "The tests passed.",
    ] {
        let dir = project("invalid", "assert_eq!(2 + 2, 4);");
        let Some((out, t, calls)) = go(reply, &dir, &[]).await else {
            return;
        };
        assert!(!out.status.success(), "{reply}: {t}");
        assert_eq!(calls, 1, "{reply}: no retry, no repair");
        assert_eq!(field(&t, "Executions:"), "0", "{reply}");
        assert_eq!(field(&t, "Observations:"), "0", "{reply}");
        assert_eq!(field(&t, "Evidence records:"), "0", "{reply}");
        assert_eq!(
            field(&t, "PAX status:"),
            "none (no valid result was observed)",
            "{reply}"
        );
        assert!(
            !dir.join("target").exists(),
            "{reply}: Cargo ran, so PAX was reached"
        );
        assert!(!t.contains("ExecutionStarted"), "{reply}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pax_that_is_not_pax_fails_closed_before_any_model_call() {
    // The real POSIX archive utility, named explicitly: identity verification refuses it.
    if !Path::new("/bin/pax").is_file() {
        eprintln!("SKIPPED: no /bin/pax on this machine");
        return;
    }
    let dir = project("posix", "assert_eq!(2 + 2, 4);");
    let (out, t, calls) = go(REQUEST, &dir, &[("PAX_BIN", "/bin/pax")]).await.unwrap();
    assert_eq!(out.status.code(), Some(3), "{t}");
    assert!(
        t.contains("PAX unavailable") && t.contains("/bin/pax is not PAX"),
        "{t}"
    );
    assert_eq!(calls, 0, "no model was asked: nothing could be run");
    assert!(!dir.join("target").exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_older_pax_fails_closed_before_any_model_call() {
    use std::os::unix::fs::PermissionsExt;
    let dir = project("oldpax", "assert_eq!(2 + 2, 4);");
    let shim_dir = unique("oldpax-shim");
    let shim = shim_dir.join("pax");
    std::fs::write(&shim, "#!/bin/sh\necho 'pax 0.2.0'\n").unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (out, t, calls) = go(REQUEST, &dir, &[("PAX_BIN", shim.to_str().unwrap())])
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{t}");
    assert!(
        t.contains("PAX unavailable") && t.contains("is PAX 0.2.0") && t.contains("0.3.0 or later"),
        "{t}"
    );
    assert_eq!(calls, 0, "no model was asked: nothing could be run");
    assert!(!dir.join("target").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn no_pax_at_all_fails_closed() {
    let dir = project("nopax", "assert_eq!(2 + 2, 4);");
    let empty = unique("emptypath");
    let (out, t, calls) = go(REQUEST, &dir, &[("PATH", empty.to_str().unwrap())])
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{t}");
    assert!(
        t.contains("PAX unavailable") && t.contains("no `pax` on the search path"),
        "{t}"
    );
    assert_eq!(calls, 0);
    assert!(!dir.join("target").exists());
}

#[test]
fn the_work_directory_is_required_and_never_comes_from_a_model() {
    let out = Command::new(env!("CARGO_BIN_EXE_chip-cli"))
        .arg("--test-pax-work")
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "m")
        .env("CHIP_ENDPOINT", "http://127.0.0.1:1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
}

/// Opt-in: a real model, real PAX, a real project. The model is neither forced nor replaced. This
/// is an integration proof, not a model-quality experiment: it asserts the chain, and that the
/// terminal state follows what PAX established, never that the model chose well.
#[test]
fn real_model_requests_pax_and_real_pax_runs() {
    if std::env::var("CHIP_TEST_REAL_MODEL").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set CHIP_TEST_REAL_MODEL=1 to call a real model");
        return;
    }
    for (tag, body, passes) in [
        ("live-pass", "assert_eq!(2 + 2, 4);", true),
        ("live-fail", "assert_eq!(2 + 2, 5);", false),
    ] {
        let dir = project(tag, body);
        let out = Command::new(env!("CARGO_BIN_EXE_chip-cli"))
            .arg("--test-pax-work")
            .arg("--workdir")
            .arg(&dir)
            .arg("--print-reply")
            .output()
            .unwrap();
        let t = text(&out);
        if out.status.code() == Some(3) {
            eprintln!("SKIPPED: {}", t.trim());
            return;
        }
        assert!(out.status.success(), "{tag}: {t}");
        assert_eq!(field(&t, "Model calls:"), "1", "{tag}");
        assert_eq!(field(&t, "Executions:"), "1", "{tag}");
        assert_eq!(field(&t, "Observations:"), "1", "{tag}");
        assert_eq!(field(&t, "Receipt:"), "none (PAX issues no receipt)");
        assert!(dir.join("target").exists(), "{tag}: Cargo really ran");
        let status = field(&t, "PAX status:");
        assert_eq!(status == "passed", passes, "{tag}: PAX status {status}");
        assert_eq!(
            field(&t, "Terminal:") == "Completed",
            passes,
            "{tag}: terminal must follow PAX: {t}"
        );
        assert_eq!(field(&t, "Goal satisfied:"), passes.to_string(), "{tag}");
        eprintln!(
            "{tag}: provider {} model {} pax_status {status} exit_code {} terminal {}",
            field(&t, "Provider:"),
            field(&t, "Model:"),
            field(&t, "Native exit code:"),
            field(&t, "Terminal:")
        );
    }
}
