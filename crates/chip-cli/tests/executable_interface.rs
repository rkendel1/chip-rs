//! The executable interface other programs (a launcher, a packager, an environment provider)
//! depend on. These are the contract: if one changes, whoever consumes the `chip` artifact has to
//! know. Nothing here refers to any particular consumer.

use std::process::{Command, Output};

fn chip() -> Command {
    Command::new(env!("CARGO_BIN_EXE_chip"))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn project() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-interface-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn one() -> u8 { 1 }\n").unwrap();
    dir
}

fn worker(root: &std::path::Path, request: Option<&str>) -> Output {
    let mut c = chip();
    c.args(["capability-exec", "--root"])
        .arg(root)
        .env_remove("CHIP_CAPABILITY_REQUEST");
    if let Some(request) = request {
        c.env("CHIP_CAPABILITY_REQUEST", request);
    }
    c.output().unwrap()
}

#[test]
fn version_is_the_executable_name_and_the_package_version() {
    for flag in ["--version", "-V"] {
        let out = chip().arg(flag).output().unwrap();
        assert!(out.status.success());
        assert_eq!(
            text(&out.stdout),
            format!("chip {}\n", env!("CARGO_PKG_VERSION"))
        );
        assert!(out.stderr.is_empty());
    }
}

#[test]
fn the_service_flags_are_stable_and_name_no_consumer() {
    let out = chip().args(["serve", "--no-such-flag"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let usage = text(&out.stderr);
    for flag in [
        "--host",
        "--port",
        "--max-concurrent-work",
        "--max-queued-work",
    ] {
        assert!(usage.contains(flag), "{flag} in {usage}");
    }
    assert!(usage.contains("usage: chip serve"), "{usage}");
    assert!(!usage.to_ascii_lowercase().contains("compute"), "{usage}");
}

#[test]
fn the_capability_worker_protocol_v1_is_what_an_environment_runs() {
    // The names an environment provider relies on.
    assert_eq!(chip_remote_env::WORKER_SUBCOMMAND, "capability-exec");
    assert_eq!(chip_remote_env::WORKER_ENV, "CHIP_CAPABILITY_REQUEST");
    let root = project();

    // capabilities: the nine Rust Chip capabilities, as one JSON object on stdout.
    let out = worker(&root, Some(r#"{"v":1,"op":"capabilities"}"#));
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(text(&out.stdout).trim()).unwrap();
    assert_eq!(
        (v["v"].as_u64(), v["op"].as_str()),
        (Some(1), Some("capabilities"))
    );
    let ids: Vec<&str> = v["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    for id in [
        "project.read",
        "project.write",
        "project.list",
        "project.search",
        "project.git.status",
        "project.git.diff",
        "project.git.diff_stat",
        "project.git.log",
        "pax.test",
    ] {
        assert!(ids.contains(&id), "{id} in {ids:?}");
    }

    // execute: Rust Chip's own executor acts on the project and answers for the execution it got.
    let request = r#"{"v":1,"op":"execute","execution_id":"x1","capability":"project.read","inputs":{"path":{"text":"src/lib.rs"}}}"#;
    let out = worker(&root, Some(request));
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_str(text(&out.stdout).trim()).unwrap();
    assert_eq!(v["result"]["execution_id"], "x1");
    assert_eq!(v["result"]["status"], "success");
    assert!(
        v["result"]["output"]
            .as_str()
            .unwrap()
            .contains("pub fn one")
    );

    // info: where the project is, for the observer's checks only.
    let out = worker(&root, Some(r#"{"v":1,"op":"info"}"#));
    let v: serde_json::Value = serde_json::from_str(text(&out.stdout).trim()).unwrap();
    assert_eq!(v["op"], "info");
    assert!(
        v["root"]
            .as_str()
            .unwrap()
            .ends_with(root.file_name().unwrap().to_str().unwrap())
    );
}

#[test]
fn a_bad_worker_invocation_fails_closed_and_prints_no_result() {
    let root = project();
    for request in [
        None,
        Some(""),
        Some("not json"),
        Some(r#"{"v":2,"op":"info"}"#),
        Some(r#"{"v":1,"op":"shell","argv":["sh"]}"#),
        Some(r#"{"v":1,"op":"execute","capability":"project.read"}"#),
    ] {
        let out = worker(&root, request);
        assert_eq!(out.status.code(), Some(2), "{request:?}");
        assert!(out.stdout.is_empty(), "{request:?}");
    }
    // Arguments are exactly `--root <dir>`: nothing else is accepted.
    for args in [
        &["capability-exec"][..],
        &["capability-exec", "--root"],
        &["capability-exec", "--root", ".", "x"],
        &["capability-exec", "--dir", "."],
    ] {
        let out = chip()
            .args(args)
            .env("CHIP_CAPABILITY_REQUEST", r#"{"v":1,"op":"info"}"#)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn a_request_cannot_make_the_worker_run_anything_but_a_declared_capability() {
    let root = project();
    // An undeclared capability, and extra inputs on a declared one, are refused by Rust Chip's own
    // executor exactly as they are locally: never run, never repaired.
    for request in [
        r#"{"v":1,"op":"execute","execution_id":"x","capability":"shell.exec","inputs":{"command":{"text":"id"}}}"#,
        r#"{"v":1,"op":"validate","capability":"project.list","inputs":{"path":{"text":"."},"executable":{"text":"/bin/sh"}}}"#,
    ] {
        let out = worker(&root, Some(request));
        assert_eq!(out.status.code(), Some(0));
        let v: serde_json::Value = serde_json::from_str(text(&out.stdout).trim()).unwrap();
        let refused = v.get("error").is_some() || v["valid"] == false;
        assert!(refused, "{v}");
    }
}
