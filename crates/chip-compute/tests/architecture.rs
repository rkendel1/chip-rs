//! Dependency-graph rules around the Compute adapter.

use std::fs;
use std::path::PathBuf;

fn deps(krate: &str) -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join(krate)
        .join("Cargo.toml");
    let manifest = fs::read_to_string(path).unwrap();
    let mut in_deps = false;
    let mut names = Vec::new();
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_deps = line.contains("dependencies");
        } else if in_deps {
            if let Some((name, _)) = line.split_once('=') {
                names.push(name.trim().to_lowercase());
            }
        }
    }
    names
}

fn mentions_compute(names: &[String]) -> Vec<&String> {
    names.iter().filter(|n| n.contains("compute")).collect()
}

#[test]
fn core_crates_do_not_depend_on_compute() {
    for krate in ["chip-core", "fx-core", "fx-provider-http"] {
        let found = deps(krate);
        assert!(
            mentions_compute(&found).is_empty(),
            "{krate} must not depend on Compute: {found:?}"
        );
    }
}

#[test]
fn chip_compute_depends_on_chip_core_only_one_way() {
    let found = deps("chip-compute");
    assert!(found.contains(&"chip-core".to_string()));
    for forbidden in ["chip-cli", "fx-provider-http"] {
        assert!(
            !found.contains(&forbidden.to_string()),
            "chip-compute must not depend on {forbidden}"
        );
    }
    // Compute is reached through its CLI at runtime, not linked as a crate;
    // no Compute crate (or state/PAX/AppPort crate) is a dependency.
    assert!(
        mentions_compute(&found)
            .iter()
            .all(|n| n.as_str() == "chip-compute" || false)
    );
}

#[test]
fn chip_core_does_not_depend_on_chip_compute() {
    assert!(!deps("chip-core").contains(&"chip-compute".to_string()));
    assert!(!deps("fx-core").contains(&"chip-compute".to_string()));
    assert!(!deps("fx-provider-http").contains(&"chip-compute".to_string()));
}

#[test]
fn chip_compute_has_no_state_or_shell_abstractions() {
    let src =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap();
    for forbidden in [
        "ShellExecutor",
        "CommandExecutor",
        "ProcessExecutor",
        "ShellCommand",
        "rusqlite",
        "HashMap<ExecutionId",
    ] {
        assert!(!src.contains(forbidden), "unexpected {forbidden}");
    }
}
