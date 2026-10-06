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
