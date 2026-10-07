//! `chip-cli serve`, the binary run as a local application would run it, driven over real TCP.
//!
//! The model is a local mock HTTP server with one fixed reply (a request that escapes the project,
//! which Chip refuses: a deterministic, bounded run) and PAX is a shim that only identifies itself,
//! so the whole suite needs no live model and no real PAX. Mocked means mocked: nothing here
//! claims a real model or a real PAX test run.

#[path = "../../fx-provider-http/tests/common/mod.rs"]
mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::Value;

fn project(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-serve-cli-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn one() -> u8 { 1 }\n").unwrap();
    dir
}

#[cfg(unix)]
fn pax_shim(tag: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("chip-serve-pax-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let shim = dir.join("pax");
    std::fs::write(&shim, "#!/bin/sh\necho 'pax 9.9.9'\n").unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    shim
}

struct Server {
    child: Child,
    addr: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn completion(content: &str) -> String {
    serde_json::json!({
        "id": "resp-serve",
        "choices": [{"message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 4},
    })
    .to_string()
}

fn command(dir: &PathBuf, model_url: &str, shim: &PathBuf) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_chip-cli"));
    c.arg("serve")
        .current_dir(dir)
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "mock-model")
        .env("CHIP_ENDPOINT", model_url)
        .env("CHIP_API_KEY", "sk-serve-secret-never-printed")
        .env("PAX_BIN", shim);
    c
}

fn launch(mut c: Command, args: &[&str]) -> Server {
    let mut child = c
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let addr = line
        .trim()
        .strip_prefix("Chip Runtime Service listening on http://")
        .unwrap_or_else(|| panic!("unexpected first line: {line:?}"))
        .to_string();
    Server { child, addr }
}

fn http(addr: &str, method: &str, path: &str, body: Option<&str>) -> (u16, String, Value) {
    let body = body.unwrap_or("");
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    (
        head.split(' ').nth(1).unwrap().parse().unwrap(),
        head.to_ascii_lowercase(),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

const ESCAPE: &str = r#"{"decision":"request_capability","capability":"project.write","inputs":{"path":"../chip-serve-escaped.txt","content":"pwned"}}"#;

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_service_runs_work_over_http_through_the_same_runtime() {
    let model = common::start(200, &completion(ESCAPE), Duration::ZERO).await;
    let (dir, shim, url) = (project("accept"), pax_shim("accept"), model.url.clone());
    let outside = dir.parent().unwrap().join("chip-serve-escaped.txt");
    let _ = std::fs::remove_file(&outside);

    tokio::task::spawn_blocking(move || {
        // Default flags are not used here only because 8765 may be taken; the default is unit-tested.
        let server = launch(
            command(&dir, &url, &shim),
            &["--host", "127.0.0.1", "--port", "0"],
        );
        let addr = server.addr.clone();
        assert!(addr.starts_with("127.0.0.1:"), "{addr}");

        // curl http://127.0.0.1:PORT/health
        let (status, head, body) = http(&addr, "GET", "/health", None);
        assert_eq!((status, body), (200, serde_json::json!({"status": "ok"})));
        assert!(!head.contains("access-control"));

        // curl -X POST /v1/work -d '{"goal":"..."}'
        let (status, _, body) = http(
            &addr,
            "POST",
            "/v1/work",
            Some(r#"{"goal":"Add a function that sorts the payload."}"#),
        );
        assert_eq!(status, 202, "{body}");
        let id = body["work_id"].as_str().unwrap().to_string();
        assert!(id.starts_with("work_"));

        // curl /v1/work/<id> until the runtime reports an end.
        let mut state = Value::Null;
        for _ in 0..500 {
            let (status, _, body) = http(&addr, "GET", &format!("/v1/work/{id}"), None);
            assert_eq!(status, 200);
            if body["status"] != "running" {
                state = body;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(state["status"], "blocked", "{state}");
        assert_eq!(state["lifecycle"], "blocked");
        assert_eq!(state["result"]["verified"], false);
        assert_eq!(state["result"]["audit"]["clean"], true);
        assert_eq!(state["result"]["measurement"]["executions"], 0);
        assert!(!state.to_string().contains("sk-serve-secret"));

        // curl /v1/work/<id>/events
        let (status, _, body) = http(&addr, "GET", &format!("/v1/work/{id}/events"), None);
        assert_eq!(status, 200);
        let kinds: Vec<&str> = body["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds.first(), Some(&"WorkStarted"));
        assert_eq!(kinds.last(), Some(&"WorkBlocked"));
        assert!(!kinds.contains(&"ExecutionStarted"), "{kinds:?}");
        assert!(!kinds.contains(&"EvidenceRecorded"), "{kinds:?}");

        assert_eq!(http(&addr, "GET", "/v1/work/work_nope", None).0, 404);
        assert_eq!(
            http(
                &addr,
                "POST",
                "/v1/work",
                Some(r#"{"goal":"x","executable":"sh"}"#)
            )
            .0,
            400
        );
        assert!(!outside.exists(), "a file was written outside the project");
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "pub fn one() -> u8 { 1 }\n"
        );
    })
    .await
    .unwrap();
    assert_eq!(
        model.captured.lock().await.len(),
        1,
        "one model call, no retry"
    );
}

#[cfg(unix)]
#[test]
fn without_a_model_or_pax_nothing_listens() {
    let dir = project("unavailable");
    let shim = pax_shim("unavailable");
    let out = command(&dir, "http://127.0.0.1:1", &shim)
        .env_remove("CHIP_MODEL")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(out.stdout.is_empty(), "it must not claim to be listening");

    let out = command(&dir, "http://127.0.0.1:1", &shim)
        .env("PAX_BIN", "/nonexistent/pax")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("PAX unavailable"));
    assert!(out.stdout.is_empty());
}

#[cfg(unix)]
#[test]
fn usage_errors_are_exit_2() {
    let dir = project("usage");
    let shim = pax_shim("usage");
    for args in [
        &["--port", "notaport"][..],
        &["--host", "not-an-ip"],
        &["--model", "m"],
        &["--port"],
        &["extra"],
    ] {
        let out = command(&dir, "http://127.0.0.1:1", &shim)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty());
    }
}
