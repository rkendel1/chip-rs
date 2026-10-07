//! CLI acceptance test verifying the complete path through the deterministic CLI.

use std::process::Command;

#[test]
fn cli_test_mode_succeeds() {
    let output = Command::new("cargo")
        .args(["run", "-p", "chip-cli", "--", "--test"])
        .output()
        .expect("CLI should execute");

    assert!(
        output.status.success(),
        "CLI exit code should be 0. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_test_mode_produces_expected_output() {
    let output = Command::new("cargo")
        .args(["run", "-p", "chip-cli", "--quiet", "--", "--test"])
        .output()
        .expect("CLI should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("Chip"),
        "output should contain 'Chip'. Got: {}",
        stdout
    );
    assert!(
        stdout.contains("FX provider: test"),
        "output should contain 'FX provider: test'. Got: {}",
        stdout
    );
    assert!(
        stdout.contains("Turn completed"),
        "output should contain 'Turn completed'. Got: {}",
        stdout
    );
}

#[test]
fn cli_test_execution_mode_uses_test_executor() {
    let output = Command::new("cargo")
        .args(["run", "-p", "chip-cli", "--quiet", "--", "--test-execution"])
        .output()
        .expect("CLI should execute");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("test execution completed"), "{stdout}");
}

#[test]
fn cli_test_decision_mode_runs_the_decision_path() {
    let output = Command::new("cargo")
        .args(["run", "-p", "chip-cli", "--quiet", "--", "--test-decision"])
        .output()
        .expect("CLI should execute");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Decision: request capability test.operation"),
        "{stdout}"
    );
    assert!(stdout.contains("test execution completed"), "{stdout}");
}

#[test]
fn cli_test_turn_mode_runs_the_full_lifecycle() {
    let output = Command::new("cargo")
        .args(["run", "-p", "chip-cli", "--quiet", "--", "--test-turn"])
        .output()
        .expect("CLI should execute");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "Decision: request capability test.operation",
        "Execution Success: test execution completed",
        "Events: 8",
        "Turn completed",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in {stdout}"
        );
    }
}

#[test]
fn cli_test_observation_mode_prints_the_observation() {
    let output = Command::new("cargo")
        .args([
            "run",
            "-p",
            "chip-cli",
            "--quiet",
            "--",
            "--test-observation",
        ])
        .output()
        .expect("CLI should execute");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "Execution: success",
        "Observation: execution.completed",
        "Execution ID: observation-1",
        "Receipt: sha256:test-receipt",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in {stdout}"
        );
    }
}

#[test]
fn cli_test_cycle_mode_runs_the_bounded_cycle() {
    let output = Command::new("cargo")
        .args(["run", "-p", "chip-cli", "--quiet", "--", "--test-cycle"])
        .output()
        .expect("CLI should execute");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "bounded cycle",
        "Turn 1: capability requested (test.operation)",
        "Execution: success",
        "Observation: execution.completed",
        "Turn 2: response (I was told the execution completed.)",
        "Cycle completed",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in {stdout}"
        );
    }
}

#[test]
fn cli_test_workload_proves_the_bounded_cycle_offline() {
    let output = Command::new("cargo")
        .args(["run", "-p", "chip-cli", "--quiet", "--", "--test-workload"])
        .output()
        .expect("CLI should execute");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "Decision: request compute.selftest",
        "Kind: execution.completed",
        "Response: observed the execution result",
        "Model calls: 2",
        "Executions: 1",
        "Observations: 1",
        "Automatic follow-ups: 0",
        "Bounded workload completed.",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in {stdout}"
        );
    }
}

/// Live path: either really ran on Compute (with a receipt) or explicitly skipped.
/// It must never claim completion when skipped, nor fall back to a test executor.
#[test]
fn cli_test_real_cycle_is_real_or_explicitly_skipped() {
    let output = Command::new("cargo")
        .args([
            "run",
            "-p",
            "chip-cli",
            "--quiet",
            "--",
            "--test-real-cycle",
        ])
        .output()
        .expect("CLI should execute");
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains("SKIPPED — Compute unavailable") {
        assert_eq!(
            output.status.code(),
            Some(3),
            "skip has its own exit status"
        );
        assert!(!stdout.contains("Bounded workload completed."), "{stdout}");
        eprintln!("SKIPPED — Compute unavailable");
    } else {
        assert!(output.status.success(), "{stdout}");
        assert!(stdout.contains("(real Compute)"), "{stdout}");
        assert!(stdout.contains("Receipt: sha256:"), "{stdout}");
        assert!(stdout.contains("Bounded workload completed."), "{stdout}");
        eprintln!("PASSED — real Compute execution");
    }
}

#[test]
fn cli_test_evidence_reuses_without_executing() {
    let output = Command::new("cargo")
        .args(["run", "-p", "chip-cli", "--quiet", "--", "--test-evidence"])
        .output()
        .expect("CLI should execute");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "Execution: performed",
        "Evidence: reused",
        "Execution: skipped",
        "Model calls added: 0",
        "Executions: 1",
        "Evidence reuses: 1",
        "Local fast path completed.",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in {stdout}"
        );
    }
}

#[test]
fn cli_test_real_evidence_is_real_or_explicitly_skipped() {
    let output = Command::new("cargo")
        .args([
            "run",
            "-p",
            "chip-cli",
            "--quiet",
            "--",
            "--test-real-evidence",
        ])
        .output()
        .expect("CLI should execute");
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains("SKIPPED — Compute unavailable") {
        assert_eq!(output.status.code(), Some(3));
        assert!(!stdout.contains("Local fast path completed."), "{stdout}");
    } else {
        assert!(output.status.success(), "{stdout}");
        assert!(stdout.contains("Receipt: sha256:"), "{stdout}");
        assert!(stdout.contains("Evidence: reused"), "{stdout}");
        assert!(stdout.contains("Executions: 1"), "{stdout}");
    }
}

#[test]
fn cli_test_evidence_validity_detects_a_state_change() {
    let output = Command::new("cargo")
        .args([
            "run",
            "-p",
            "chip-cli",
            "--quiet",
            "--",
            "--test-evidence-validity",
        ])
        .output()
        .expect("CLI should execute");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "State: F1",
        "State changed: F2",
        "Evidence: stale",
        "Executions: 2",
        "Evidence reuses: 2",
        "Stale: 1",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in {stdout}"
        );
    }
}

#[test]
fn cli_test_local_reasoner_orders_evidence_reasoning_and_escalation() {
    let output = Command::new("cargo")
        .args([
            "run",
            "-p",
            "chip-cli",
            "--quiet",
            "--",
            "--test-local-reasoner",
        ])
        .output()
        .expect("CLI should execute");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "Evidence: valid\nLocal reasoning: skipped\nFX calls: 0\nExecution: 0",
        "Evidence: stale\nLocal reasoning: continue",
        "FX calls: 0\nExecution: 0",
        "Evidence: unknown\nLocal reasoning: escalate",
        "FX escalation: explicit",
        "Local reasoner proof completed.",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in {stdout}"
        );
    }
}

/// Correctness and accounting only; timings are never asserted.
#[test]
fn cli_benchmark_local_reasoner_is_correct_and_accounted_for() {
    let output = Command::new("cargo")
        .args([
            "run",
            "-p",
            "chip-cli",
            "--quiet",
            "--",
            "--benchmark-local-reasoner",
            "60",
        ])
        .output()
        .expect("CLI should execute");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "Local Reasoning Benchmark",
        "Cases: 180",
        "Rust:",
        "WASM:",
        "FX:",
        "Evidence (hit):",
        "Evidence (stale) + WASM:",
        "model calls: 180",
        "executions: 0",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in {stdout}"
        );
    }
    assert!(!stdout.contains("executions: 1"), "{stdout}");
}

#[test]
fn cli_benchmark_live_reasoner_skips_without_configuration() {
    let output = Command::new("cargo")
        .args([
            "run",
            "-p",
            "chip-cli",
            "--quiet",
            "--",
            "--benchmark-live-reasoner",
        ])
        .env_remove("CHIP_MODEL")
        .env_remove("CHIP_ENDPOINT")
        .output()
        .expect("CLI should execute");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("SKIPPED — live provider not configured"),
        "{stdout}"
    );
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn cli_test_reasoning_corpus_replays_rust_and_wasm_offline() {
    let output = Command::new("cargo")
        .args([
            "run",
            "-p",
            "chip-cli",
            "--quiet",
            "--",
            "--test-reasoning-corpus",
        ])
        .output()
        .expect("CLI should execute");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "Reasoning Corpus",
        "Cases: 32",
        "Rust:",
        "WASM:",
        "Rust/WASM agreement: 32/32",
        "False continues: 0",
        "Model calls: 0",
        "Executions: 0",
        "Evidence writes: 0",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in {stdout}"
        );
    }
    assert!(
        stdout.contains("Case: lj-01"),
        "baseline mismatches must be surfaced: {stdout}"
    );
}

/// Optional infrastructure: either the real model is available (and the demo runs) or the
/// command reports SKIPPED with its own exit status. Never a false pass.
#[test]
fn cli_local_model_reasoner_is_real_or_explicitly_skipped() {
    let output = Command::new("cargo")
        .args([
            "run",
            "-p",
            "chip-cli",
            "--quiet",
            "--",
            "--test-local-model-reasoner",
        ])
        .output()
        .expect("CLI should execute");
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains("SKIPPED — local model unavailable") {
        assert_eq!(output.status.code(), Some(3));
        assert!(!stdout.contains("Native Local Model Reasoner"), "{stdout}");
    } else {
        assert!(output.status.success(), "{stdout}");
        assert!(
            stdout.contains("KnownValid: evidence reused, local model calls: 0"),
            "{stdout}"
        );
        assert!(stdout.contains("Executions: 0") && stdout.contains("Remote model calls: 0"));
    }
}

#[test]
fn cli_corpus_reports_the_native_model_as_evaluated_or_skipped() {
    let output = Command::new("cargo")
        .args([
            "run",
            "-p",
            "chip-cli",
            "--quiet",
            "--",
            "--test-reasoning-corpus",
        ])
        .output()
        .expect("CLI should execute");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Native model: skipped — ") || stdout.contains("Native model:\n  Correct:"),
        "{stdout}"
    );
    assert!(stdout.contains("Rust/WASM agreement: 32/32"));
}

/// Experimental Laya: either it runs on a local checkpoint, or it is explicitly skipped (exit 3).
#[test]
fn cli_test_laya_reasoner_is_real_or_explicitly_skipped() {
    let output = Command::new("cargo")
        .args([
            "run",
            "-p",
            "chip-cli",
            "--quiet",
            "--",
            "--test-laya-reasoner",
        ])
        .env_remove("CHIP_LAYA_MODEL_DIR")
        .output()
        .expect("CLI should execute");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Laya reasoner: SKIPPED\nReason: "),
        "{stdout}"
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(
        !stdout.contains("Laya Decision Reasoner"),
        "no results when skipped: {stdout}"
    );
}

mod graph_snapshot {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../chip-graph/tests/fixture")
    }

    fn copy_fixture(name: &str) -> PathBuf {
        let dest = std::env::temp_dir().join(format!("chip-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        let status = Command::new("cp")
            .arg("-r")
            .arg(fixture())
            .arg(&dest)
            .status()
            .unwrap();
        assert!(status.success());
        dest
    }

    fn run(args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_chip-cli"))
            .args(args)
            .output()
            .unwrap()
    }

    fn text(o: &Output) -> String {
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    #[test]
    fn init_writes_a_snapshot_and_graph_reads_it() {
        let dir = copy_fixture("init");
        let root = dir.to_str().unwrap();

        let missing = run(&["graph", "--root", root]);
        assert!(text(&missing).contains("No architecture snapshot found.\nRun `chip init`."));

        let init = run(&["init", "--root", root]);
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        let out = text(&init);
        for needle in [
            "Crates: 2",
            "Binaries: 1",
            "Snapshot: sha256:",
            "Written: .chip/graph/sha256-",
        ] {
            assert!(out.contains(needle), "missing {needle} in {out}");
        }
        assert!(
            !out.contains(root),
            "init output must not print the absolute path"
        );

        let graph = run(&["graph", "--root", root]);
        assert!(graph.status.success());
        let shown = text(&graph);
        assert!(
            shown.contains("Architecture Graph") && shown.contains("Implements: 1"),
            "{shown}"
        );

        // `graph` reads the stored snapshot; it does not rebuild from source.
        std::fs::write(dir.join("src/extra.rs"), "pub fn extra() {}\n").unwrap();
        assert_eq!(text(&run(&["graph", "--root", root])), shown);

        // A real change to the repository is a new snapshot.
        let second = text(&run(&["init", "--root", root]));
        assert_ne!(
            second.lines().find(|l| l.starts_with("Snapshot:")),
            out.lines().find(|l| l.starts_with("Snapshot:"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_benchmark_reports_timings() {
        let dir = copy_fixture("bench");
        let out = text(&run(&[
            "init",
            "--benchmark",
            "--root",
            dir.to_str().unwrap(),
        ]));
        for needle in [
            "Files scanned:",
            "Parse time:",
            "Graph construction time:",
            "Serialization time:",
            "Total:",
            "Snapshot size:",
        ] {
            assert!(out.contains(needle), "missing {needle} in {out}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
