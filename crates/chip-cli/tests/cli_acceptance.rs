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
