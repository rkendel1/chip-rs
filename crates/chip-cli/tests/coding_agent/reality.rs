//! Reality checks the evaluation runs *itself*, outside any model loop: it builds the project,
//! runs its tests with Cargo, runs PAX directly and runs the compiled `courier` binary. What a
//! model said about the work is never an input here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use chip_pax::{PaxExecutionResult, parse_execution_result};

#[derive(Debug, Clone, Default)]
pub struct CargoTests {
    /// `binary::test` to `ok` / `FAILED` / `ignored`.
    pub results: BTreeMap<String, String>,
}

impl CargoTests {
    pub fn total(&self) -> usize {
        self.results.len()
    }
    pub fn failed(&self) -> Vec<&String> {
        self.results
            .iter()
            .filter(|(_, s)| *s == "FAILED")
            .map(|(k, _)| k)
            .collect()
    }
}

/// `cargo test --offline --no-fail-fast` in `dir`, parsed per test.
pub fn cargo_tests(dir: &Path) -> CargoTests {
    // Cargo prints the `Running <binary>` lines on stderr and the test lines on stdout; merging
    // them through the shell keeps each test attached to the binary that ran it.
    let out = Command::new("sh")
        .args(["-c", "cargo test --offline --no-fail-fast 2>&1"])
        .current_dir(dir)
        .output()
        .expect("cargo runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut results = BTreeMap::new();
    let mut binary = String::from("?");
    for line in text.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("Running ") {
            binary = rest.split_whitespace().next().unwrap_or("?").to_string();
        } else if let Some((name, status)) = line
            .strip_prefix("test ")
            .and_then(|rest| rest.rsplit_once(" ... "))
        {
            let status = status.split_whitespace().next().unwrap_or("?");
            results.insert(format!("{binary}::{name}"), status.to_string());
        }
    }
    CargoTests { results }
}

/// Builds the `courier` binary and returns its path, or `None` if the project does not build.
pub fn build_binary(dir: &Path) -> Option<PathBuf> {
    let ok = Command::new("cargo")
        .args(["build", "--offline", "--quiet", "--bin", "courier"])
        .current_dir(dir)
        .output()
        .ok()?
        .status
        .success();
    let bin = dir.join("target/debug/courier");
    (ok && bin.exists()).then_some(bin)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub fn run_binary(bin: &Path, args: &[&str]) -> Ran {
    let out = Command::new(bin)
        .args(args)
        .env_clear()
        .output()
        .expect("the binary runs");
    Ran {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// PAX, run directly (not through Chip's capability) on `dir`.
pub fn pax_direct(pax: &Path, dir: &Path) -> Result<PaxExecutionResult, String> {
    let out = Command::new(pax)
        .arg("--dir")
        .arg(dir)
        .args(["--json", "test"])
        .output()
        .map_err(|e| e.to_string())?;
    parse_execution_result(&out.stdout).map_err(|e| e.to_string())
}

/// A configuration file outside the project, so writing it cannot dirty the tree under test.
pub fn probe_config(tag: &str, text: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("chip-e2e-probe-{}-{tag}.conf", std::process::id()));
    std::fs::write(&path, text).unwrap();
    path
}
