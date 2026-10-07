//! PR45: `chip-cli work "<goal>"`, the binary run as a user would run it, in the project directory.
//!
//! Mock provider tests drive the real `HttpProvider` against a local HTTP server that answers one
//! fixed reply, with the real filesystem and real PAX/Cargo behind it (the multi-step scripted
//! trajectories are in the crate's own tests). The one live test is opt-in
//! (`CHIP_TEST_REAL_MODEL=1`): a real model, nothing forced, nothing scripted, real files, real PAX,
//! real Cargo. It asserts that the runtime's invariants held and that what Chip reported matches
//! what an independent run of PAX says about the project afterwards. It does not assert that the
//! model succeeded: that is the measurement, not the test.

#[path = "../../fx-provider-http/tests/common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const GOAL: &str = "Add a function `canonical_fingerprint` that returns the canonical form of a payload, so the project's tests pass.";

const OLD_LIB: &str = "pub fn payload_len(payload: &str) -> usize {\n    payload.len()\n}\n";
const TESTS: &str = "use fpfixture::canonical_fingerprint;\n\n#[test]\nfn pairs_are_sorted_and_joined() {\n    assert_eq!(canonical_fingerprint(\"b=2&a=1\"), \"a=1&b=2\");\n}\n\n#[test]\nfn an_already_canonical_payload_is_unchanged() {\n    assert_eq!(canonical_fingerprint(\"a=1&b=2\"), \"a=1&b=2\");\n}\n\n#[test]\nfn a_single_pair_is_unchanged() {\n    assert_eq!(canonical_fingerprint(\"z=9\"), \"z=9\");\n}\n";

fn fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-work-cli-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"fpfixture_{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"fpfixture\"\n",
            tag.replace('-', "_")
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), OLD_LIB).unwrap();
    std::fs::write(dir.join("tests/fingerprint.rs"), TESTS).unwrap();
    dir
}

fn completion(content: &str) -> String {
    serde_json::json!({
        "id": "resp-work",
        "choices": [{"message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 4},
    })
    .to_string()
}

fn base(args: &[&str], dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_chip-cli"));
    c.arg("work")
        .args(args)
        .current_dir(dir)
        .env_remove("PAX_BIN");
    c
}

fn mock(url: &str, mut c: Command) -> Command {
    c.env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "mock-model")
        .env("CHIP_ENDPOINT", url)
        .env("CHIP_API_KEY", "sk-work-secret-never-printed");
    c
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

async fn go(reply: &str, dir: &Path, args: &[&str]) -> (Output, usize) {
    let server = common::start(200, &completion(reply), Duration::ZERO).await;
    let (url, d, a): (String, PathBuf, Vec<String>) = (
        server.url.clone(),
        dir.to_path_buf(),
        args.iter().map(|s| s.to_string()).collect(),
    );
    let out = tokio::task::spawn_blocking(move || {
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        mock(&url, base(&a, &d)).output().unwrap()
    })
    .await
    .unwrap();
    let calls = server.captured.lock().await.len();
    (out, calls)
}

fn pax_installed() -> bool {
    Command::new("pax")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn json_of(o: &Output) -> serde_json::Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {}", text(o)))
}

#[test]
fn usage_errors_are_exit_2_and_run_nothing() {
    let dir = fixture("usage");
    for args in [
        &[][..],
        &[""],
        &["   "],
        &["goal one", "goal two"],
        &["--bogus", "goal"],
        &["goal", "--max-turns"],
        &["goal", "--max-turns", "0"],
        &["goal", "--max-turns", "51"],
        &["goal", "--max-turns", "many"],
        &["goal", "--max-executions", "-1"],
        &["goal", "--dir", "/"],
        &["goal", "--workdir", "/tmp"],
    ] {
        let out = mock("http://127.0.0.1:1", base(args, &dir))
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", text(&out));
        assert!(text(&out).contains("usage: chip-cli work"), "{args:?}");
    }
    let long = "x".repeat(2001);
    let out = mock("http://127.0.0.1:1", base(&[&long], &dir))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!dir.join("target").exists());
}

#[test]
fn without_a_provider_nothing_runs() {
    let dir = fixture("noprovider");
    let out = base(&[GOAL], &dir)
        .env_remove("CHIP_MODEL")
        .env_remove("CHIP_PROVIDER")
        .env_remove("CHIP_ENDPOINT")
        .env_remove("CHIP_API_KEY")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert_eq!(
        std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
        OLD_LIB
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_usable_pax_nothing_is_attempted_and_no_model_is_asked() {
    let dir = fixture("nopax");
    let empty = std::env::temp_dir().join(format!("chip-work-empty-{}", std::process::id()));
    std::fs::create_dir_all(&empty).unwrap();
    let server = common::start(200, &completion("{}"), Duration::ZERO).await;
    let (url, d, e) = (server.url.clone(), dir.clone(), empty.clone());
    let out = tokio::task::spawn_blocking(move || {
        mock(&url, base(&[GOAL], &d))
            .env("PATH", e)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(text(&out).contains("PAX unavailable"), "{}", text(&out));
    assert_eq!(server.captured.lock().await.len(), 0);
    assert!(out.stdout.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_that_escapes_the_project_is_refused_and_nothing_changes() {
    if !pax_installed() {
        eprintln!("SKIPPED: PAX is not installed");
        return;
    }
    let dir = fixture("escape");
    let outside = dir
        .parent()
        .unwrap()
        .join(format!("chip-work-escaped-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&outside);
    let reply = format!(
        r#"{{"decision":"request_capability","capability":"project.write","inputs":{{"path":"../{}","content":"pwned"}}}}"#,
        outside.file_name().unwrap().to_string_lossy()
    );
    let (out, calls) = go(&reply, &dir, &["--json", GOAL]).await;
    assert_eq!(calls, 1, "one model call, no retry");
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let j = json_of(&out);
    assert_eq!(j["terminal_state"], "blocked");
    assert_eq!(j["verified"], false);
    assert_eq!(j["writes"], 0);
    assert_eq!(j["measurement"]["executions"], 0);
    assert_eq!(j["measurement"]["observations"], 0);
    assert_eq!(j["audit"]["clean"], true);
    assert!(!outside.exists(), "a file was written outside the project");
    assert_eq!(
        std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
        OLD_LIB
    );
    assert!(!dir.join("target").exists());
    assert!(!text(&out).contains("sk-work-secret"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_decision_is_a_runtime_failure() {
    if !pax_installed() {
        eprintln!("SKIPPED: PAX is not installed");
        return;
    }
    let dir = fixture("malformed");
    let (out, calls) = go("All done! The tests pass.", &dir, &["--json", GOAL]).await;
    assert_eq!(calls, 1);
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    assert_eq!(json_of(&out)["terminal_state"], "failed");
    assert_eq!(json_of(&out)["measurement"]["executions"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_that_only_claims_completion_is_not_believed() {
    if !pax_installed() {
        eprintln!("SKIPPED: PAX is not installed");
        return;
    }
    let dir = fixture("claim");
    let (out, _) = go(
        r#"{"decision":"complete","summary":"I added it; the tests pass."}"#,
        &dir,
        &[GOAL],
    )
    .await;
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(t.starts_with("Work blocked.\n"), "{t}");
    assert!(t.contains("completion refused"), "{t}");
    assert!(t.contains("Verified: no"), "{t}");
    assert!(t.contains("Last test result: none"), "{t}");
    for line in [
        "Audit: clean",
        "  unauthorized executions: 0",
        "  path escapes: 0",
        "  out-of-root writes: 0",
        "  forged observations: 0",
        "  false completions: 0",
    ] {
        assert!(t.contains(line), "{line:?} missing from:\n{t}");
    }
    assert!(t.contains("Executed: none"), "{t}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_single_read_is_real_and_the_goal_is_still_unmet() {
    if !pax_installed() {
        eprintln!("SKIPPED: PAX is not installed");
        return;
    }
    let dir = fixture("read");
    // The mock answers the same read every turn; the work ends at the turn bound, not in success.
    let (out, calls) = go(
        r#"{"decision":"request_capability","capability":"project.read","inputs":{"path":"src/lib.rs"}}"#,
        &dir,
        &["--json", "--max-turns", "3", GOAL],
    )
    .await;
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert_eq!(calls, 3, "one call per turn, the bound enforced");
    let j = json_of(&out);
    assert_eq!(j["terminal_state"], "limit_reached");
    assert_eq!(
        j["reads"], 3,
        "every read was performed again, never remembered"
    );
    assert_eq!(j["executions_by_capability"]["project.read"], 3);
    assert_eq!(j["verified"], false);
    assert_eq!(j["audit"]["clean"], true);
}

// ---- live ---------------------------------------------------------------------------------------------------

/// Opt-in: a real model against the fixture project, unscripted. Asserts the invariants and that the
/// report agrees with an independent run of PAX over the project's real state afterwards.
#[test]
fn real_model_does_real_work() {
    if std::env::var("CHIP_TEST_REAL_MODEL").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set CHIP_TEST_REAL_MODEL=1 to call a real model");
        return;
    }
    let dir = fixture("live");
    let out = Command::new(env!("CARGO_BIN_EXE_chip-cli"))
        .arg("work")
        .args(["--json", "--print-reply", GOAL])
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
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {t}"));
    // Always: the boundary held, whatever the model did.
    assert_eq!(j["audit"]["clean"], true, "{t}");
    for c in [
        "unauthorized_executions",
        "unauthorized_completions",
        "false_completions",
        "evidence_without_observation",
        "observation_without_execution",
        "execution_without_valid_request",
        "limit_violations",
        "path_escape",
        "out_of_root_write",
    ] {
        assert_eq!(j["audit"][c], 0, "{c}: {t}");
    }
    assert_eq!(j["receipt"], serde_json::Value::Null);
    // The report matches reality: independently, ask PAX about the project as it now is.
    let independent = Command::new("pax")
        .args(["--dir"])
        .arg(&dir)
        .args(["--json", "test"])
        .output()
        .unwrap();
    let independent: serde_json::Value =
        serde_json::from_slice(&independent.stdout).unwrap_or_default();
    let really_passes = independent["status"] == "passed";
    let verified = j["verified"].as_bool().unwrap();
    if verified {
        assert!(
            really_passes,
            "Chip says verified but PAX says {independent}: {t}"
        );
        assert_eq!(j["terminal_state"], "completed");
        assert_eq!(out.status.code(), Some(0));
        assert!(j["changed_writes"].as_u64().unwrap() >= 1);
        assert_eq!(
            std::fs::read_to_string(dir.join("tests/fingerprint.rs")).unwrap(),
            TESTS,
            "the model rewrote the tests it was to satisfy"
        );
    } else {
        assert_ne!(j["terminal_state"], "completed");
        assert_ne!(out.status.code(), Some(0));
    }
    let m = &j["measurement"];
    eprintln!(
        "live: terminal {} verified {} exit {:?} | turns {} model_calls {} executions {} (reads {} writes {} changed {} useful {} pax {}) failed_obs {} recoveries {} | independent PAX {} | tokens {} latency total {}ms model {}ms exec {}ms | paths {} | outcome {}",
        j["terminal_state"],
        verified,
        out.status.code(),
        m["turns"],
        m["model_calls"],
        m["executions"],
        j["reads"],
        j["writes"],
        j["changed_writes"],
        j["useful_writes"],
        j["pax_executions"],
        j["failed_observations"],
        j["recoveries"],
        independent["status"],
        m["model_tokens"],
        m["total_latency_ms"],
        m["model_latency_ms"],
        m["compute_latency_ms"],
        j["paths_written"],
        j["outcome_reason"],
    );
}

// ---- PR46: the selected model is the only model ------------------------------------------------------------

/// Runs `work` against a mock whose endpoint is given on the command line, with the environment
/// pointing somewhere else entirely: the command line must win, and nothing else may be called.
#[tokio::test(flavor = "multi_thread")]
async fn the_command_line_selection_is_the_only_model_called() {
    if !pax_installed() {
        eprintln!("SKIPPED: PAX is not installed");
        return;
    }
    let dir = fixture("select");
    let chosen = common::start(
        200,
        &completion(r#"{"decision":"block","reason":"stop"}"#),
        Duration::ZERO,
    )
    .await;
    let decoy = common::start(
        200,
        &completion(r#"{"decision":"block","reason":"decoy"}"#),
        Duration::ZERO,
    )
    .await;
    let (url, decoy_url, d) = (chosen.url.clone(), decoy.url.clone(), dir.clone());
    let out = tokio::task::spawn_blocking(move || {
        base(
            &[
                "--json",
                "--provider",
                "openai-compatible",
                "--model",
                "cli-model",
                "--endpoint",
                &url,
                GOAL,
            ],
            &d,
        )
        .env("CHIP_PROVIDER", "anthropic")
        .env("CHIP_MODEL", "env-model")
        .env("CHIP_ENDPOINT", &decoy_url)
        .env("CHIP_API_KEY", "sk-select-secret-never-printed")
        .output()
        .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(chosen.captured.lock().await.len(), 1, "{}", text(&out));
    assert_eq!(
        decoy.captured.lock().await.len(),
        0,
        "the environment's endpoint was called"
    );
    let j = json_of(&out);
    assert_eq!(
        (j["provider"].as_str(), j["model"].as_str()),
        (Some("openai-compatible"), Some("cli-model"))
    );
    let identity = j["endpoint"].as_str().unwrap();
    assert!(
        identity.starts_with("http://127.0.0.1:") && !identity.contains("/v1"),
        "{identity}"
    );
    let all = text(&out);
    assert!(
        !all.contains("sk-select-secret") && !all.contains(dir.to_str().unwrap()),
        "a secret or host path was printed"
    );
    assert_eq!(j["terminal_state"], "blocked");
}

#[test]
fn a_model_is_never_inferred_from_the_provider() {
    let dir = fixture("nomodel");
    for provider in ["ollama", "anthropic"] {
        let out = base(&["--provider", provider, GOAL], &dir)
            .env_remove("CHIP_MODEL")
            .env_remove("CHIP_PROVIDER")
            .env_remove("CHIP_ENDPOINT")
            .env_remove("CHIP_OLLAMA_ENDPOINT")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(3), "{provider}: {}", text(&out));
        assert!(
            text(&out).contains("no model is selected") && text(&out).contains("CHIP_MODEL"),
            "{}",
            text(&out)
        );
        assert!(out.stdout.is_empty());
    }
    assert!(!dir.join("target").exists());
}

#[test]
fn flags_that_need_a_value_say_so() {
    let dir = fixture("flagvalue");
    for args in [
        &["--provider"][..],
        &["--model", "--json"],
        &["--endpoint", ""],
        &["--provider", "  ", "goal"],
    ] {
        let mut full = args.to_vec();
        if !full.contains(&"goal") {
            full.push("goal");
        }
        let out = mock("http://127.0.0.1:1", base(&full, &dir))
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", text(&out));
    }
}

/// An explicitly selected Ollama that is not running fails explicitly, and nothing else is tried:
/// not another provider, not another model, not an endpoint from the environment.
#[tokio::test(flavor = "multi_thread")]
async fn an_unavailable_provider_fails_explicitly_with_no_fallback() {
    if !pax_installed() {
        eprintln!("SKIPPED: PAX is not installed");
        return;
    }
    let dir = fixture("nofallback");
    // The environment offers a working model; the command line selects one that is not there.
    let working = common::start(
        200,
        &completion(r#"{"decision":"block","reason":"x"}"#),
        Duration::ZERO,
    )
    .await;
    let (url, d) = (working.url.clone(), dir.clone());
    let out = tokio::task::spawn_blocking(move || {
        base(
            &[
                "--json",
                "--provider",
                "ollama",
                "--model",
                "qwen3-coder",
                "--endpoint",
                "http://127.0.0.1:1",
                GOAL,
            ],
            &d,
        )
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "fallback-model")
        .env("CHIP_ENDPOINT", &url)
        .env("CHIP_OLLAMA_ENDPOINT", &url)
        .output()
        .unwrap()
    })
    .await
    .unwrap();
    let t = text(&out);
    assert_eq!(out.status.code(), Some(3), "{t}");
    assert!(
        t.contains("the selected model did not answer")
            && t.contains("ollama")
            && t.contains("qwen3-coder"),
        "{t}"
    );
    assert!(t.contains("No other provider or model was tried"), "{t}");
    assert_eq!(
        working.captured.lock().await.len(),
        0,
        "a fallback model was called"
    );
    assert!(
        out.stdout.is_empty(),
        "no verdict is printed for a run that never began"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
        OLD_LIB
    );
}

// ---- PR46: live coding tasks, model chosen on the command line --------------------------------------------------

const AREA_LIB: &str = "/// Area of a rectangle.\npub fn rect_area(width: u32, height: u32) -> u32 {\n    width + height\n}\n\n/// Perimeter of a rectangle.\npub fn rect_perimeter(width: u32, height: u32) -> u32 {\n    2 * (width + height)\n}\n";
const AREA_TESTS: &str = "use shapes::{rect_area, rect_perimeter};\n\n#[test]\nfn area_is_width_times_height() {\n    assert_eq!(rect_area(3, 4), 12);\n}\n\n#[test]\nfn a_zero_side_has_no_area() {\n    assert_eq!(rect_area(0, 5), 0);\n}\n\n#[test]\nfn perimeter_is_twice_the_sides() {\n    assert_eq!(rect_perimeter(3, 4), 14);\n}\n";

const ROWS_LIB: &str = "pub mod parse;\npub mod render;\n";
const ROWS_PARSE: &str = "/// Splits one input line into trimmed fields.\npub fn fields(line: &str) -> Vec<String> {\n    line.split(';').map(|f| f.trim().to_string()).collect()\n}\n";
const ROWS_RENDER: &str = "/// Renders fields as one table row.\npub fn table_row(fields: &[String]) -> String {\n    fields.join(\",\")\n}\n";
const ROWS_TESTS: &str = "use rowfmt::parse::fields;\nuse rowfmt::render::table_row;\n\n#[test]\nfn fields_are_split_on_commas_and_trimmed() {\n    assert_eq!(fields(\"a, b ,c\"), vec![\"a\", \"b\", \"c\"]);\n}\n\n#[test]\nfn a_row_is_joined_with_pipes() {\n    assert_eq!(table_row(&[\"a\".to_string(), \"b\".to_string()]), \"a | b\");\n}\n\n#[test]\nfn parsing_then_rendering() {\n    assert_eq!(table_row(&fields(\"x,y\")), \"x | y\");\n}\n";

/// A real Rust project: a library name, its source files, and its tests directory files.
fn project(tag: &str, lib: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-live-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{lib}_{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"{lib}\"\n", tag.replace('-', "_")),
    )
    .unwrap();
    for (path, text) in files {
        std::fs::write(dir.join(path), text).unwrap();
    }
    dir
}

fn task(name: &str, tag: &str) -> (PathBuf, &'static str) {
    match name {
        "a" => (
            project(
                tag,
                "shapes",
                &[("src/lib.rs", AREA_LIB), ("tests/shapes.rs", AREA_TESTS)],
            ),
            "Fix the failing tests in this project.",
        ),
        "b" => (
            project(
                tag,
                "rowfmt",
                &[
                    ("src/lib.rs", ROWS_LIB),
                    ("src/parse.rs", ROWS_PARSE),
                    ("src/render.rs", ROWS_RENDER),
                    ("tests/rows.rs", ROWS_TESTS),
                ],
            ),
            "Fix the failing tests in this project.",
        ),
        "fp" => (
            project(
                tag,
                "fpfixture",
                &[("src/lib.rs", OLD_LIB), ("tests/fingerprint.rs", TESTS)],
            ),
            GOAL,
        ),
        other => panic!("unknown task {other}"),
    }
}

fn tests_of(name: &str) -> Vec<(&'static str, &'static str)> {
    match name {
        "a" => vec![("tests/shapes.rs", AREA_TESTS)],
        "b" => vec![("tests/rows.rs", ROWS_TESTS)],
        _ => vec![("tests/fingerprint.rs", TESTS)],
    }
}

/// Opt-in. Which model is chosen on the command line, from `CHIP_LIVE_PROVIDER` and `CHIP_LIVE_MODEL`
/// (and `CHIP_LIVE_TASKS`, default `a,b,fp`): the run is real, unscripted and unforced. The test
/// asserts the invariants and that the report agrees with an independent PAX run over the project
/// as it then is. It does not assert that the model succeeded: that is the measurement.
#[test]
fn real_model_codes_real_tasks() {
    if std::env::var("CHIP_TEST_REAL_MODEL").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set CHIP_TEST_REAL_MODEL=1 to call a real model");
        return;
    }
    let provider = std::env::var("CHIP_LIVE_PROVIDER").ok();
    let model = std::env::var("CHIP_LIVE_MODEL").ok();
    let tasks = std::env::var("CHIP_LIVE_TASKS").unwrap_or_else(|_| "a,b,fp".into());
    for name in tasks.split(',') {
        let (dir, goal) = task(
            name,
            &format!(
                "{name}-{}",
                model.as_deref().unwrap_or("env").replace([':', '.'], "-")
            ),
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_chip-cli"));
        command.arg("work").args(["--json", "--print-reply"]);
        if let Some(p) = &provider {
            command.args(["--provider", p]);
        }
        if let Some(m) = &model {
            command.args(["--model", m]);
        }
        for (var, flag) in [
            ("CHIP_LIVE_MAX_TURNS", "--max-turns"),
            ("CHIP_LIVE_MAX_EXECUTIONS", "--max-executions"),
        ] {
            if let Ok(n) = std::env::var(var) {
                command.args([flag, &n]);
            }
        }
        let out = command
            .arg(goal)
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
            serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{name}: {e}: {t}"));
        // Always: the boundary held, whatever the model did.
        assert_eq!(j["audit"]["clean"], true, "{name}: {t}");
        for c in [
            "unauthorized_executions",
            "unauthorized_completions",
            "false_completions",
            "evidence_without_observation",
            "observation_without_execution",
            "execution_without_valid_request",
            "limit_violations",
            "stale_evidence_reuse",
            "events_after_terminal",
            "path_escape",
            "out_of_root_write",
            "host_path_leak",
            "navigation_mismatch",
            "forged_observations",
        ] {
            assert_eq!(j["audit"][c], 0, "{name} {c}: {t}");
        }
        assert_eq!(j["receipt"], serde_json::Value::Null);
        assert!(
            !t.contains(dir.to_str().unwrap()) || t.contains("Model reply"),
            "a host path was printed outside model replies"
        );
        // The report agrees with an independent PAX run over the project as it now is.
        let independent = Command::new("pax")
            .arg("--dir")
            .arg(&dir)
            .args(["--json", "test"])
            .output()
            .unwrap();
        let independent: serde_json::Value =
            serde_json::from_slice(&independent.stdout).unwrap_or_default();
        let verified = j["verified"].as_bool().unwrap();
        if verified {
            assert_eq!(
                independent["status"], "passed",
                "{name}: Chip says verified, PAX says {independent}: {t}"
            );
            assert_eq!(j["terminal_state"], "completed");
            assert_eq!(out.status.code(), Some(0));
            for (path, original) in tests_of(name) {
                assert_eq!(
                    std::fs::read_to_string(dir.join(path)).unwrap(),
                    original,
                    "{name}: the model rewrote the tests it was to satisfy"
                );
            }
        } else {
            assert_ne!(j["terminal_state"], "completed");
            assert_ne!(out.status.code(), Some(0));
        }
        let m = &j["measurement"];
        eprintln!(
            "LIVE task={name} provider={} model={} endpoint={} terminal={} verified={verified} turns={} calls={} executions={} lists={} searches={} reads={} writes={} tests={} failed_obs={} recoveries={} useful_per_call={} useful_per_exec={} tokens={} latency_total_ms={} model_ms={} exec_ms={} independent_pax={} outcome={}",
            j["provider"],
            j["model"],
            j["endpoint"],
            j["terminal_state"],
            m["turns"],
            m["model_calls"],
            m["executions"],
            j["lists"],
            j["searches"],
            j["reads"],
            j["writes"],
            j["tests"],
            j["failed_observations"],
            j["recoveries"],
            j["useful_work_per_model_call"],
            j["useful_work_per_execution"],
            m["model_tokens"],
            m["total_latency_ms"],
            m["model_latency_ms"],
            m["compute_latency_ms"],
            independent["status"],
            j["outcome_reason"],
        );
    }
}
