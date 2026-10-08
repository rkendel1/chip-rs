//! The compiled `courier` binary.

use std::process::Command;

fn courier(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_courier"))
        .args(args)
        .env_remove("COURIER_CLIENT_TIMEOUT")
        .output()
        .expect("the binary runs")
}

#[test]
fn help_exits_zero() {
    let out = courier(&["help"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("usage: courier"));
}

#[test]
fn send_prints_the_response() {
    let out = courier(&["send", "GET", "http://h/hello"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("GET /hello"));
}

#[test]
fn a_terminal_failure_exits_one() {
    let out = courier(&["send", "GET", "http://h/status/404"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn usage_errors_exit_two() {
    assert_eq!(courier(&["nonsense"]).status.code(), Some(2));
}

#[test]
fn a_bad_configuration_exits_three() {
    let dir = std::env::temp_dir().join(format!("courier-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bad.conf");
    std::fs::write(&path, "[client]\ntimeout = 0ms\n").unwrap();
    let out = courier(&["--config", path.to_str().unwrap(), "config", "check"]);
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn config_check_lists_warnings() {
    let dir = std::env::temp_dir().join(format!("courier-cli-warn-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("warn.conf");
    std::fs::write(&path, "[client]\nfuture = 1\n").unwrap();
    let out = courier(&["--config", path.to_str().unwrap(), "config", "check"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("configuration is valid"));
    assert!(text.contains("warning: [client] unknown key `future` ignored"));
}

#[test]
fn the_journal_survives_between_invocations() {
    let dir = std::env::temp_dir().join(format!("courier-cli-journal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let journal = dir.join("j.log");
    let j = journal.to_str().unwrap();
    assert!(courier(&["--journal", j, "send", "GET", "http://h/a"]).status.success());
    assert!(courier(&["--journal", j, "send", "GET", "http://h/b"]).status.success());
    let stats = courier(&["--journal", j, "journal", "stats"]);
    assert!(String::from_utf8_lossy(&stats.stdout).contains("entries 2"));
}
