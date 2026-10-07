//! PR44: `chip-cli verify`, the standalone software-verification agent.
//!
//! The binary is run as a user would run it: from the project directory. The model provider is a
//! mock HTTP server (the real `HttpProvider` talks to it) except in the opt-in live test
//! (`CHIP_TEST_REAL_MODEL=1`), which uses the configured real model. PAX 0.3.0 and Cargo are real
//! wherever PAX can produce the case; the malformed-result and old-version cases use a small script
//! at the PAX process boundary and say so. If PAX is not installed a test says SKIPPED and returns.

#[path = "../../fx-provider-http/tests/common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

fn completion(content: &str) -> String {
    serde_json::json!({
        "id": "resp-verify",
        "choices": [{"message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 4},
    })
    .to_string()
}

fn unique(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-verify-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn rust_project(tag: &str, lib: &str) -> PathBuf {
    let dir = unique(tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"verifyfixture_{tag}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), lib).unwrap();
    dir
}

fn project(tag: &str, test_body: &str) -> PathBuf {
    rust_project(
        tag,
        &format!(
            "#[cfg(test)]\nmod tests {{\n    #[test]\n    fn the_test() {{\n        {test_body}\n    }}\n}}\n"
        ),
    )
}

fn passing(tag: &str) -> PathBuf {
    project(tag, "assert_eq!(2 + 2, 4);")
}
fn failing(tag: &str) -> PathBuf {
    project(tag, "assert_eq!(2 + 2, 5);")
}
fn zero_tests(tag: &str) -> PathBuf {
    rust_project(tag, "pub fn nothing_to_test() {}\n")
}

const REQUEST: &str =
    r#"{"schema":"chip.work-decision.v1","decision":"request_capability","capability":"pax.test"}"#;

fn run(url: &str, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chip-cli"));
    command
        .arg("verify")
        .args(args)
        .current_dir(dir)
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "mock-model")
        .env("CHIP_ENDPOINT", url)
        .env("CHIP_API_KEY", "sk-verify-secret-never-printed")
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
        .find_map(|l| l.strip_prefix(name))
        .unwrap_or_else(|| panic!("no `{name}` in:\n{out}"))
        .trim()
        .to_string()
}

struct Ran {
    out: Output,
    text: String,
    model_calls_seen_by_server: usize,
}

/// Runs `verify` in `dir` against a mock model that answers `reply`. `None` if PAX is unavailable.
async fn go(reply: &str, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Option<Ran> {
    let server = common::start(200, &completion(reply), Duration::ZERO).await;
    let url = server.url.clone();
    let (d, a, e): (PathBuf, Vec<String>, Vec<(String, String)>) = (
        dir.to_path_buf(),
        args.iter().map(|s| s.to_string()).collect(),
        envs.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    );
    let out = tokio::task::spawn_blocking(move || {
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        let e: Vec<(&str, &str)> = e.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        run(&url, &d, &a, &e)
    })
    .await
    .unwrap();
    let t = text(&out);
    if out.status.code() == Some(3) && t.contains("PAX unavailable") && envs.is_empty() {
        eprintln!("SKIPPED: {}", t.trim());
        return None;
    }
    let calls = server.captured.lock().await.len();
    Some(Ran {
        out,
        text: t,
        model_calls_seen_by_server: calls,
    })
}

fn json_of(r: &Ran) -> serde_json::Value {
    serde_json::from_slice(&r.out.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {}", r.text))
}

const CLEAN: &str = "Audit: clean";

// ---- the three real outcomes -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_passing_project_is_verified() {
    let dir = passing("pass");
    let Some(r) = go(REQUEST, &dir, &[], &[]).await else {
        return;
    };
    let t = &r.text;
    assert_eq!(r.out.status.code(), Some(0), "{t}");
    assert_eq!(r.model_calls_seen_by_server, 1, "one model call, no retry");
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines[0], "Verification completed.");
    assert_eq!(lines[1], "Goal: Verify that the project's tests pass.");
    assert_eq!(lines[2], "Capability: pax.test");
    assert_eq!(lines[3], "Result: passed");
    assert_eq!(lines[4], "Tests: 1 passed, 0 failed, 0 ignored");
    assert!(
        field(t, "Work:")
            .starts_with("1 model call(s), 1 execution(s), 1 observation(s), verified outputs 1/1"),
        "{t}"
    );
    assert!(
        field(t, "Evidence:").contains("no cryptographic execution receipt"),
        "{t}"
    );
    assert!(t.contains(CLEAN), "{t}");
    assert!(!t.contains("Reason:"), "a pass needs no reason line");
    assert!(dir.join("target").exists(), "Cargo really ran, through PAX");
    assert!(
        !t.contains("sk-verify-secret"),
        "no credential in the output"
    );
    // Tokens come from the provider's own report, never an estimate.
    assert_eq!(field(t, "Tokens:"), "15");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_project_is_blocked() {
    let dir = failing("fail");
    let Some(r) = go(REQUEST, &dir, &[], &[]).await else {
        return;
    };
    let t = &r.text;
    // Native exit code 101 is in the observation; Chip's own status is "not verified".
    assert_eq!(r.out.status.code(), Some(1), "{t}");
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines[0], "Verification blocked.");
    assert_eq!(lines[2], "Capability: pax.test");
    assert_eq!(lines[3], "Result: failed");
    assert_eq!(lines[4], "Reason: tests-failed");
    assert_eq!(lines[5], "Tests: 0 passed, 1 failed, 0 ignored");
    assert!(field(t, "Native exit code:").starts_with("101 "), "{t}");
    assert!(field(t, "Work:").contains("verified outputs 0/1"), "{t}");
    assert!(t.contains(CLEAN), "{t}");
    assert!(!t.contains("Verification completed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_with_no_tests_is_blocked_even_though_the_native_exit_code_is_zero() {
    let dir = zero_tests("zero");
    let Some(r) = go(REQUEST, &dir, &[], &[]).await else {
        return;
    };
    let t = &r.text;
    assert_eq!(
        r.out.status.code(),
        Some(1),
        "an exit code of 0 is not verification: {t}"
    );
    assert_eq!(field(t, "Result:"), "not_run");
    assert_eq!(field(t, "Reason:"), "no-tests-executed");
    assert!(field(t, "Native exit code:").starts_with("0 "), "{t}");
    assert!(t.starts_with("Verification blocked."), "{t}");
    assert!(field(t, "Work:").contains("verified outputs 0/1"), "{t}");
}

// ---- machine-readable output ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn json_output_carries_the_verdict_pax_s_fields_and_the_existing_measurement() {
    for (tag, dir, state, status, reason, exit, satisfied, code) in [
        (
            "json-pass",
            passing("json-pass"),
            "completed",
            "passed",
            "tests-passed",
            0,
            true,
            0,
        ),
        (
            "json-fail",
            failing("json-fail"),
            "blocked",
            "failed",
            "tests-failed",
            101,
            false,
            1,
        ),
        (
            "json-zero",
            zero_tests("json-zero"),
            "blocked",
            "not_run",
            "no-tests-executed",
            0,
            false,
            1,
        ),
    ] {
        let Some(r) = go(REQUEST, &dir, &["--json"], &[]).await else {
            return;
        };
        assert_eq!(r.out.status.code(), Some(code), "{tag}: {}", r.text);
        let j = json_of(&r);
        assert_eq!(j["command"], "verify", "{tag}");
        assert_eq!(j["goal"], "Verify that the project's tests pass.");
        assert_eq!(j["terminal_state"], state, "{tag}");
        assert_eq!(j["goal_satisfied"], satisfied, "{tag}");
        assert_eq!(j["capability"], "pax.test");
        assert_eq!(j["pax"]["status"], status, "{tag}");
        assert_eq!(j["pax"]["reason"], reason, "{tag}");
        assert_eq!(j["pax"]["tool"], "cargo");
        assert_eq!(j["pax"]["exit_code"], exit, "{tag}");
        assert!(
            j["pax"]["version"].as_str().unwrap().starts_with("0."),
            "{tag}"
        );
        assert_eq!(
            j["receipt"],
            serde_json::Value::Null,
            "{tag}: no receipt is invented"
        );
        assert_eq!(j["verified_outputs"], u64::from(satisfied), "{tag}");
        assert_eq!(j["required_outputs"], 1);
        assert_eq!(j["exit_status"], code, "{tag}");
        assert_eq!(j["audit"]["clean"], true, "{tag}");
        for counter in [
            "unauthorized_executions",
            "unauthorized_completions",
            "false_completions",
            "evidence_without_observation",
            "observation_without_execution",
            "execution_without_valid_request",
            "limit_violations",
        ] {
            assert_eq!(j["audit"][counter], 0, "{tag}: {counter}");
        }
        // The existing measurement format, nested as is.
        let m = &j["measurement"];
        assert_eq!(m["workload"], "verify");
        assert_eq!(m["model_calls"], 1, "{tag}");
        assert_eq!(m["executions"], 1, "{tag}");
        assert_eq!(m["observations"], 1, "{tag}");
        assert_eq!(m["model_tokens"], 15, "{tag}");
        // Only one JSON document on stdout; nothing else.
        assert!(
            String::from_utf8_lossy(&r.out.stdout)
                .trim()
                .lines()
                .count()
                == 1,
            "{tag}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn passing_json_has_test_counts_and_failing_json_has_them_too() {
    let dir = failing("json-counts");
    let Some(r) = go(REQUEST, &dir, &["--json"], &[]).await else {
        return;
    };
    let t = &json_of(&r)["pax"]["tests"];
    assert_eq!(
        (
            t["passed"].as_u64(),
            t["failed"].as_u64(),
            t["ignored"].as_u64()
        ),
        (Some(0), Some(1), Some(0))
    );
}

// ---- the boundary: nothing invalid reaches PAX -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn model_authored_invocations_never_reach_pax() {
    for reply in [
        r#"{"capability":"pax.test","inputs":{}}"#,
        r#"{"decision":"request_capability","capability":"pax.test","inputs":{}}"#,
        r#"{"decision":"request_capability","capability":"pax.test","command":"cargo test"}"#,
        r#"{"decision":"request_capability","capability":"pax.test","inputs":{"command":"cargo test"}}"#,
        r#"{"decision":"request_capability","capability":"pax.test","executable":"/bin/sh"}"#,
        r#"{"decision":"request_capability","capability":"pax.test","args":["--help"]}"#,
        r#"{"decision":"request_capability","capability":"pax.test","working_directory":"/"}"#,
        r#"{"decision":"request_capability","capability":"pax.test","status":"passed"}"#,
        r#"{"decision":"request_capability","capability":"pax.test","exit_code":0}"#,
        r#"{"decision":"request_capability","capability":"pax.test","observation":"passed"}"#,
        r#"{"decision":"request_capability","capability":"pax.test","receipt":"sha256:x"}"#,
        r#"{"decision":"request_capability","capability":"pax.build"}"#,
        r#"{"decision":"request_capability","capability":"pax.exec"}"#,
        "The tests passed.",
    ] {
        let dir = passing("invalid");
        let Some(r) = go(reply, &dir, &["--json"], &[]).await else {
            return;
        };
        let t = &r.text;
        assert_ne!(r.out.status.code(), Some(0), "{reply}: {t}");
        assert_eq!(
            r.model_calls_seen_by_server, 1,
            "{reply}: no retry, no repair"
        );
        let j = json_of(&r);
        assert_eq!(j["measurement"]["executions"], 0, "{reply}");
        assert_eq!(j["measurement"]["observations"], 0, "{reply}");
        assert_eq!(j["capability"], serde_json::Value::Null, "{reply}");
        assert_eq!(j["pax"]["status"], serde_json::Value::Null, "{reply}");
        assert_ne!(j["terminal_state"], "completed", "{reply}");
        eprintln!(
            "invalid: {reply} -> {} exit {:?} ({})",
            j["terminal_state"],
            r.out.status.code(),
            j["outcome_reason"]
        );
        assert_eq!(j["audit"]["clean"], true, "{reply}");
        assert!(
            !dir.join("target").exists(),
            "{reply}: Cargo ran, so PAX was reached"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_that_claims_completion_first_is_refused_and_nothing_runs() {
    let dir = passing("claim-first");
    let Some(r) = go(
        r#"{"decision":"complete","summary":"All tests passed."}"#,
        &dir,
        &["--json"],
        &[],
    )
    .await
    else {
        return;
    };
    let j = json_of(&r);
    assert_eq!(r.out.status.code(), Some(1), "{}", r.text);
    assert_eq!(j["terminal_state"], "blocked");
    assert!(
        j["outcome_reason"]
            .as_str()
            .unwrap()
            .starts_with("completion refused")
    );
    assert_eq!(j["measurement"]["executions"], 0);
    assert!(!dir.join("target").exists());
}

// ---- PAX itself unavailable, old, or producing garbage -----------------------------------------------------

#[cfg(unix)]
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test(flavor = "multi_thread")]
async fn no_pax_at_all_fails_closed_before_any_model_call() {
    let dir = passing("nopax");
    let empty = unique("emptypath");
    let r = go(REQUEST, &dir, &[], &[("PATH", empty.to_str().unwrap())])
        .await
        .unwrap();
    assert_eq!(r.out.status.code(), Some(3), "{}", r.text);
    assert!(
        r.text.contains("PAX unavailable") && r.text.contains("nothing was run"),
        "{}",
        r.text
    );
    assert_eq!(
        r.model_calls_seen_by_server, 0,
        "no model was asked: nothing could be run"
    );
    assert!(r.out.stdout.is_empty(), "no verdict on stdout");
    assert!(!dir.join("target").exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_pax_below_the_minimum_version_is_unavailable() {
    let dir = passing("oldpax");
    let shim = script(&unique("oldpax-shim"), "pax", "echo 'pax 0.2.0'");
    let r = go(REQUEST, &dir, &[], &[("PAX_BIN", shim.to_str().unwrap())])
        .await
        .unwrap();
    assert_eq!(r.out.status.code(), Some(3), "{}", r.text);
    assert!(
        r.text.contains("is PAX 0.2.0") && r.text.contains("0.3.0 or later"),
        "{}",
        r.text
    );
    assert_eq!(r.model_calls_seen_by_server, 0);
    assert!(!dir.join("target").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_posix_archive_utility_is_not_pax() {
    if !Path::new("/bin/pax").is_file() {
        eprintln!("SKIPPED: no /bin/pax on this machine");
        return;
    }
    let dir = passing("posix");
    let r = go(REQUEST, &dir, &[], &[("PAX_BIN", "/bin/pax")])
        .await
        .unwrap();
    assert_eq!(r.out.status.code(), Some(3), "{}", r.text);
    assert!(r.text.contains("/bin/pax is not PAX"), "{}", r.text);
    assert_eq!(r.model_calls_seen_by_server, 0);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_pax_result_is_never_a_verification() {
    // A PAX that identifies itself correctly, then writes something that is not a valid result.
    let dir = passing("malformed");
    let shim_dir = unique("malformed-shim");
    for (what, body) in [
        ("not JSON", "echo 'all tests passed'"),
        ("empty", "true"),
        (
            "wrong schema",
            r#"echo '{"schema":"pax.execution-result.v2","operation":"test","status":"passed","reason":"tests-passed","tool":"cargo","exit_code":0}'"#,
        ),
        (
            "invalid status",
            r#"echo '{"schema":"pax.execution-result.v1","operation":"test","status":"success","reason":"x","tool":"cargo","exit_code":0}'"#,
        ),
        (
            "exit_code a string",
            r#"echo '{"schema":"pax.execution-result.v1","operation":"test","status":"passed","reason":"tests-passed","tool":"cargo","exit_code":"0"}'"#,
        ),
    ] {
        let shim = script(
            &shim_dir,
            "pax",
            &format!(
                "if [ \"$1\" = \"--version\" ]; then echo 'pax 0.3.0'; exit 0; fi\n{body}\nexit 0"
            ),
        );
        let r = go(
            REQUEST,
            &dir,
            &["--json"],
            &[("PAX_BIN", shim.to_str().unwrap())],
        )
        .await
        .unwrap();
        assert_ne!(r.out.status.code(), Some(0), "{what}: {}", r.text);
        let j = json_of(&r);
        assert_ne!(j["terminal_state"], "completed", "{what}");
        eprintln!(
            "malformed ({what}) -> {} exit {:?} ({})",
            j["terminal_state"],
            r.out.status.code(),
            j["outcome_reason"]
        );
        // A goal that was never evaluated is null, not false and not true.
        assert!(
            j["goal_satisfied"].is_null(),
            "{what}: {}",
            j["goal_satisfied"]
        );
        assert_eq!(
            j["measurement"]["observations"], 0,
            "{what}: an observation was fabricated"
        );
        assert_eq!(j["pax"]["status"], serde_json::Value::Null, "{what}");
        assert_eq!(j["verified_outputs"], 0, "{what}");
        assert_eq!(j["audit"]["clean"], true, "{what}");
    }
}

// ---- packaging and surface --------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_standalone_path_needs_no_compute_and_no_ambient_tooling_beyond_pax_and_cargo() {
    // PATH holds only PAX's directory, Cargo's directory and the system's: no Compute, nothing else of
    // this stack. (PAX needs `cargo` to run the project's tests.)
    let dir = passing("standalone");
    let probe = go(REQUEST, &dir, &[], &[]).await;
    if probe.is_none() {
        return;
    }
    let which = |name: &str| {
        Command::new("which")
            .arg(name)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                PathBuf::from(String::from_utf8_lossy(&o.stdout).trim())
                    .parent()
                    .unwrap()
                    .to_path_buf()
            })
    };
    let (Some(pax_dir), Some(cargo_dir)) = (which("pax"), which("cargo")) else {
        return;
    };
    let path = format!(
        "{}:{}:/usr/bin:/bin",
        pax_dir.display(),
        cargo_dir.display()
    );
    assert!(
        !path
            .split(':')
            .any(|d| Path::new(d).join("compute").exists()),
        "a Compute binary is on PATH, so this proves nothing"
    );
    let dir = passing("standalone2");
    let r = go(REQUEST, &dir, &[], &[("PATH", &path)]).await.unwrap();
    assert_eq!(r.out.status.code(), Some(0), "{}", r.text);
    assert_eq!(field(&r.text, "Result:"), "passed");
}

#[test]
fn unexpected_arguments_are_a_usage_error_and_no_path_can_be_chosen() {
    for args in [
        &["--workdir", "/tmp"][..],
        &["/tmp"],
        &["--dir", "/"],
        &["test"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_chip-cli"))
            .arg("verify")
            .args(args)
            .env("CHIP_PROVIDER", "openai-compatible")
            .env("CHIP_MODEL", "m")
            .env("CHIP_ENDPOINT", "http://127.0.0.1:1")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", text(&out));
        assert!(text(&out).contains("usage: chip-cli verify"));
    }
}

#[test]
fn without_a_provider_nothing_runs_and_the_status_says_so() {
    let dir = passing("noprovider");
    let out = Command::new(env!("CARGO_BIN_EXE_chip-cli"))
        .arg("verify")
        .current_dir(&dir)
        .env_remove("CHIP_MODEL")
        .env_remove("CHIP_PROVIDER")
        .env_remove("CHIP_ENDPOINT")
        .env_remove("CHIP_API_KEY")
        .env_remove("PAX_BIN")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(!dir.join("target").exists());
}

#[test]
fn version_is_chip_s_own_and_does_not_mention_pax() {
    let out = Command::new(env!("CARGO_BIN_EXE_chip-cli"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let t = text(&out);
    assert_eq!(t.trim(), format!("chip-cli {}", env!("CARGO_PKG_VERSION")));
    assert!(!t.to_lowercase().contains("pax"));
}

// ---- live: a real model, real PAX, real Cargo ------------------------------------------------------------------------

/// Opt-in. The model is neither forced nor replaced. Asserts the chain and that the terminal state
/// follows what PAX established; never that the model chose well beyond selecting `pax.test`.
#[test]
fn real_model_verifies_passing_failing_and_zero_test_projects() {
    if std::env::var("CHIP_TEST_REAL_MODEL").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set CHIP_TEST_REAL_MODEL=1 to call a real model");
        return;
    }
    for (tag, dir, status, reason, state, code, verified) in [
        (
            "live-pass",
            passing("live-pass"),
            "passed",
            "tests-passed",
            "completed",
            0,
            1,
        ),
        (
            "live-fail",
            failing("live-fail"),
            "failed",
            "tests-failed",
            "blocked",
            1,
            0,
        ),
        (
            "live-zero",
            zero_tests("live-zero"),
            "not_run",
            "no-tests-executed",
            "blocked",
            1,
            0,
        ),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_chip-cli"))
            .arg("verify")
            .arg("--json")
            .arg("--print-reply")
            .current_dir(&dir)
            .env_remove("PAX_BIN")
            .output()
            .unwrap();
        let t = text(&out);
        if out.status.code() == Some(3) {
            eprintln!("SKIPPED: {}", t.trim());
            return;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let j: serde_json::Value =
            serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{tag}: {e}: {t}"));
        assert_eq!(out.status.code(), Some(code), "{tag}: {t}");
        assert_eq!(j["measurement"]["model_calls"], 1, "{tag}: {t}");
        assert_eq!(j["capability"], "pax.test", "{tag}");
        assert_eq!(j["pax"]["status"], status, "{tag}");
        assert_eq!(j["pax"]["reason"], reason, "{tag}");
        assert_eq!(j["terminal_state"], state, "{tag}");
        assert_eq!(j["goal_satisfied"], state == "completed", "{tag}");
        assert_eq!(j["verified_outputs"], verified, "{tag}");
        assert_eq!(j["audit"]["clean"], true, "{tag}");
        assert_eq!(j["receipt"], serde_json::Value::Null);
        // Every field the release report needs, printed for the record (no secrets, no prompts).
        let m = &j["measurement"];
        eprintln!(
            "{tag}: pax {} status {} reason {} tool {} exit_code {} tests {} | goal_satisfied {} terminal {} verified {} | model_calls {} executions {} observations {} tokens {} latency total {}ms model {}ms pax {}ms",
            j["pax"]["version"],
            j["pax"]["status"],
            j["pax"]["reason"],
            j["pax"]["tool"],
            j["pax"]["exit_code"],
            j["pax"]["tests"],
            j["goal_satisfied"],
            j["terminal_state"],
            j["verified_outputs"],
            m["model_calls"],
            m["executions"],
            m["observations"],
            m["model_tokens"],
            m["total_latency_ms"],
            m["model_latency_ms"],
            m["compute_latency_ms"],
        );
    }
}
