//! PAX is started only when a work needs it, and at most once for that work.
//!
//! `pax --version` is a subprocess. It used to run twice at the start of every `chip work`, even
//! for a work that never ran a test. These tests count the processes `chip work` really starts:
//! a counting `pax` stands at the process boundary (it answers `--version` and, for `test`, a
//! `passed` result) and records every invocation. They fail if eager probing returns.
//!
//! The model is a scripted HTTP endpoint (the reply depends on how many observations the request
//! already carries); the project, the filesystem, `git` and the `chip` binary are real.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PASSED: &str = r#"{"schema":"pax.execution-result.v1","operation":"test","status":"passed","reason":"tests-passed","tool":"cargo","exit_code":0}"#;

fn unique(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-pax-probes-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn project(tag: &str) -> PathBuf {
    let dir = unique(&format!("project-{tag}"));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn one() -> u8 { 1 }\n").unwrap();
    let _ = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .status();
    dir
}

/// A `pax` that records each invocation (`version` or `run`) and answers like PAX `version`.
struct CountingPax {
    dir: PathBuf,
    log: PathBuf,
}

impl CountingPax {
    fn new(tag: &str, version: &str) -> Self {
        let dir = unique(&format!("pax-{tag}"));
        let log = dir.join("calls.log");
        let path = dir.join("pax");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo version >> '{log}'; echo 'pax {version}'; exit 0; fi\nif [ \"$4\" = \"observe\" ]; then echo observe >> '{log}'; exit 2; fi\necho run >> '{log}'\necho '{PASSED}'\nexit 0\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir, log }
    }

    /// Acceptance: no normal work (a change, a verification, an inspection that never asked) starts
    /// project observation. Checked for every test that uses this PAX, when it is dropped.
    fn assert_never_observed(&self) {
        assert_eq!(
            self.count("observe"),
            0,
            "a work that never asked for project.observe started a PAX observation"
        );
    }

    fn count(&self, what: &str) -> usize {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == what)
            .count()
    }

    fn path_env(&self) -> std::ffi::OsString {
        let mut paths = vec![self.dir.clone()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        std::env::join_paths(paths).unwrap()
    }
}

impl Drop for CountingPax {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.assert_never_observed();
        }
    }
}

/// A chat-completions endpoint whose reply is `script(observations_so_far)`.
async fn model(script: fn(usize) -> &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let counted = counted.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                loop {
                    let Ok(n) = stream.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                counted.fetch_add(1, Ordering::SeqCst);
                let step = String::from_utf8_lossy(&buf)
                    .matches("Observation:\\nkind:")
                    .count();
                let content = serde_json::to_string(script(step)).unwrap();
                let payload = format!(
                    "{{\"id\":\"m\",\"choices\":[{{\"message\":{{\"role\":\"assistant\",\"content\":{content}}}}}],\"usage\":{{\"prompt_tokens\":1,\"completion_tokens\":1}}}}"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (url, requests)
}

const BLOCK: &str = r#"{"decision":"block","reason":"probe"}"#;

/// Reads, lists and asks Git: nothing that needs PAX.
fn no_pax_needed(step: usize) -> &'static str {
    match step {
        0 => {
            r#"{"decision":"request_capability","capability":"project.list","inputs":{"path":"."}}"#
        }
        1 => {
            r#"{"decision":"request_capability","capability":"project.read","inputs":{"path":"src/lib.rs"}}"#
        }
        2 => r#"{"decision":"request_capability","capability":"project.git.status"}"#,
        _ => BLOCK,
    }
}

/// Runs the tests twice.
fn pax_twice(step: usize) -> &'static str {
    match step {
        0 | 1 => r#"{"decision":"request_capability","capability":"pax.test"}"#,
        _ => BLOCK,
    }
}

async fn chip_work(
    dir: &Path,
    url: &str,
    path: &std::ffi::OsStr,
    extra: &[(&str, &Path)],
) -> (Option<i32>, String) {
    chip_work_args(dir, url, path, extra, &[]).await
}

async fn chip_work_args(
    dir: &Path,
    url: &str,
    path: &std::ffi::OsStr,
    extra: &[(&str, &Path)],
    args: &[&str],
) -> (Option<i32>, String) {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_chip"));
    cmd.args(["work", "probe goal"])
        .args(args)
        .current_dir(dir)
        .env_remove("PAX_BIN")
        .env("PATH", path)
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "probe")
        .env("CHIP_ENDPOINT", url)
        .stdin(Stdio::null());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let out = cmd.output().await.unwrap();
    (
        out.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_work_that_never_needs_pax_starts_no_pax_process() {
    let pax = CountingPax::new("none", "0.3.0");
    let dir = project("none");
    let (url, requests) = model(no_pax_needed).await;
    let (code, text) = chip_work(&dir, &url, &pax.path_env(), &[]).await;
    assert_eq!(code, Some(1), "blocked by the model, as scripted: {text}");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        4,
        "three executions and a stop: {text}"
    );
    assert_eq!(
        (pax.count("version"), pax.count("run")),
        (0, 0),
        "no PAX process may start for a work that never runs a test"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_work_that_runs_pax_resolves_it_once_however_often_it_is_used() {
    let pax = CountingPax::new("twice", "0.3.0");
    let dir = project("twice");
    let (url, requests) = model(pax_twice).await;
    let (code, text) = chip_work(&dir, &url, &pax.path_env(), &[]).await;
    assert_eq!(code, Some(1), "{text}");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        3,
        "two executions and a stop: {text}"
    );
    assert_eq!(pax.count("run"), 2, "the tests ran twice: {text}");
    assert_eq!(
        pax.count("version"),
        1,
        "PAX is resolved once for the work: not at startup, not per request, not per execution"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pax_that_is_too_old_fails_when_it_is_needed_and_the_tests_never_run() {
    let pax = CountingPax::new("old", "0.2.0");
    let dir = project("old");
    // A work that does not need PAX is not affected by it; one that asks for the tests is refused.
    let (url, _) = model(no_pax_needed).await;
    let (code, text) = chip_work(&dir, &url, &pax.path_env(), &[]).await;
    assert_eq!(code, Some(1), "{text}");
    assert_eq!(pax.count("version"), 0);

    let (url, requests) = model(pax_twice).await;
    let (code, text) = chip_work(&dir, &url, &pax.path_env(), &[]).await;
    assert_ne!(code, Some(0), "{text}");
    assert_eq!(pax.count("run"), 0, "the tests must never run: {text}");
    assert_eq!(
        pax.count("version"),
        1,
        "the version was checked once, when needed: {text}"
    );
    assert!(requests.load(Ordering::SeqCst) >= 1);
    assert!(
        text.contains("is PAX 0.2.0") && text.contains("0.3.0 or later"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_pax_at_all_still_stops_chip_work_before_a_model_is_asked() {
    let dir = project("missing");
    let empty = unique("empty-path");
    let (url, requests) = model(no_pax_needed).await;
    let (code, text) = chip_work(&dir, &url, empty.as_os_str(), &[]).await;
    assert_eq!(code, Some(3), "{text}");
    assert!(text.contains("PAX unavailable"), "{text}");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "no model was asked: {text}"
    );
}

/// `chip work --kind verify`: the runtime completes a passing, unchanged project itself, after one
/// model call that asked for the tests and claimed nothing. Exit status 0, PAX resolved once.
#[tokio::test(flavor = "multi_thread")]
async fn a_verify_goal_completes_a_passing_project_with_one_model_call() {
    let pax = CountingPax::new("verify", "0.3.0");
    let dir = project("verify");
    let before = std::fs::read(dir.join("src/lib.rs")).unwrap();
    let (url, requests) =
        model(|_| r#"{"decision":"request_capability","capability":"pax.test"}"#).await;
    let (code, text) = chip_work_args(
        &dir,
        &url,
        &pax.path_env(),
        &[],
        &["--kind", "verify", "--json"],
    )
    .await;
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the model asked for the tests and nothing else: {text}"
    );
    assert_eq!((pax.count("version"), pax.count("run")), (1, 1));
    let json: serde_json::Value =
        serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(json["goal_kind"], "verify");
    assert_eq!(json["terminal_state"], "completed");
    assert_eq!(json["verified"], true);
    assert_eq!(json["changed_writes"], 0);
    assert_eq!(std::fs::read(dir.join("src/lib.rs")).unwrap(), before);
}

/// `chip work --kind inspect`: a grounded answer completes; the default kind still refuses it.
#[tokio::test(flavor = "multi_thread")]
async fn an_inspect_goal_completes_on_a_grounded_answer_and_the_default_kind_does_not() {
    fn script(step: usize) -> &'static str {
        match step {
            0 => {
                r#"{"decision":"request_capability","capability":"project.read","inputs":{"path":"src/lib.rs"}}"#
            }
            _ => r#"{"decision":"complete","summary":"`one` is defined in src/lib.rs."}"#,
        }
    }
    let pax = CountingPax::new("inspect", "0.3.0");
    let dir = project("inspect");
    let (url, requests) = model(script).await;
    let (code, text) = chip_work_args(
        &dir,
        &url,
        &pax.path_env(),
        &[],
        &["--kind", "inspect", "--json"],
    )
    .await;
    assert_eq!(code, Some(1), "grounded is not verified: {text}");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    let json: serde_json::Value =
        serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(json["goal_kind"], "inspect");
    assert_eq!(json["terminal_state"], "completed");
    assert_eq!(json["goal_satisfied"], true);
    assert_eq!(json["grounded"], true);
    assert_eq!(json["verified"], false);
    assert!(json["answer"].as_str().unwrap().contains("src/lib.rs"));
    assert_eq!(
        (pax.count("version"), pax.count("run")),
        (0, 0),
        "answering needed no PAX process"
    );

    let (url, _) = model(script).await;
    let (code, text) = chip_work(&dir, &url, &pax.path_env(), &[]).await;
    assert_eq!(
        code,
        Some(1),
        "as change work the same answer is refused: {text}"
    );
    assert!(text.contains("completion refused"), "{text}");
}
