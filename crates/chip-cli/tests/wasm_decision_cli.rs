//! `chip-cli --benchmark-wasm-decision` against the real Wasm module.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chip-cli"))
        .args(args)
        .output()
        .unwrap()
}

/// Builds the module into a private target directory. `None` when the Wasm target is not
/// installed, so jobs that never installed it are not broken; CI for this PR installs it.
fn build_module() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target_dir = root.join("target/chip-cli-wasm-decision-test");
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args([
            "build",
            "-p",
            "chip-wasm-decision",
            "--target",
            "wasm32-unknown-unknown",
            "--profile",
            "wasm-decision",
            "--offline",
        ])
        .output()
        .unwrap();
    if !output.status.success() {
        eprintln!(
            "SKIPPED: could not build the Wasm module ({}); run `rustup target add wasm32-unknown-unknown`",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .last()
                .unwrap_or("")
        );
        return None;
    }
    Some(target_dir.join("wasm32-unknown-unknown/wasm-decision/chip_wasm_decision.wasm"))
}

#[test]
fn the_benchmark_reports_initialization_decision_and_end_to_end_separately() {
    let Some(wasm) = build_module() else { return };
    let out = run(&[
        "--benchmark-wasm-decision",
        "--wasm",
        wasm.to_str().unwrap(),
        "1000",
        "2000",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "optimized bytes",
        "Canonical state: 64 bytes",
        "Iterations: 1000 decisions",
        "Iterations: 2000 decisions",
        "Module initialization",
        "compile + validate module",
        "instantiate + ABI check",
        "Decision invocation (live instance)",
        "wasm: decide_bytes",
        "native: typed reference",
        "End to end (fresh instance per decision",
        "Wasm / native overhead",
        "median",
        "p95",
        "max",
        "allocates nothing",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in\n{text}");
    }
}

#[test]
fn a_missing_module_is_skipped_not_failed() {
    let out = run(&[
        "--benchmark-wasm-decision",
        "--wasm",
        "/nonexistent/decision.wasm",
    ]);
    assert_eq!(out.status.code(), Some(3));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("SKIPPED") && text.contains("cargo build -p chip-wasm-decision"));
}

#[test]
fn a_module_that_violates_the_contract_is_refused_not_benchmarked() {
    let dir = std::env::temp_dir().join(format!("chip-wasm-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("bad.wasm");
    std::fs::write(&bad, b"not wasm").unwrap();
    let out = run(&["--benchmark-wasm-decision", "--wasm", bad.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("median"));
    let _ = std::fs::remove_dir_all(&dir);
}
