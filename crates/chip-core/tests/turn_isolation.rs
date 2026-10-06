//! PR7: the lifecycle composes existing abstractions without collapsing them.

use std::fs;
use std::path::PathBuf;

fn lib() -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap()
}

/// Source of one `pub async fn` / `async fn` in the `Agent` impl.
fn function(src: &str, signature: &str) -> String {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("{signature} not found"));
    let end = src[start..].find("\n    }\n").unwrap() + start;
    src[start..end].to_lowercase()
}

#[test]
fn run_turn_touches_no_runtime_apis() {
    let src = lib();
    for signature in ["pub async fn run_turn(", "async fn decide_step("] {
        let body = function(&src, signature);
        for forbidden in [
            "compute",
            "std::process",
            "std::fs",
            "tokio",
            "reqwest",
            "command",
            "shell",
            "fx_provider_http",
            "chip_compute",
            "openai",
            "anthropic",
            "loop",
            "while ",
            "retry",
        ] {
            assert!(
                !body.contains(forbidden),
                "{signature} must not reference {forbidden}"
            );
        }
    }
}

#[test]
fn run_turn_reuses_the_single_validation_path() {
    let body = function(&lib(), "pub async fn run_turn(");
    assert!(body.contains("validate_capability_request"));
    // It must not re-implement declared/available/input checks.
    for duplicated in ["availability(", ".capabilities()", "inputs.keys"] {
        assert!(
            !body.contains(duplicated),
            "run_turn duplicates validation: {duplicated}"
        );
    }
    assert_eq!(
        body.matches("self.execute(").count(),
        1,
        "at most one execution site"
    );
}

#[test]
fn chip_core_stays_independent_of_adapters() {
    let manifest =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    for forbidden in [
        "chip-compute",
        "fx-provider-http",
        "reqwest",
        "rusqlite",
        "sqlite",
        "feltdb",
    ] {
        assert!(
            !manifest.to_lowercase().contains(forbidden),
            "chip-core must not depend on {forbidden}"
        );
    }
}

#[test]
fn no_persistence_in_chip_core() {
    let src = lib().to_lowercase();
    for forbidden in ["std::fs", "file::create", "sqlite", "journal", "checkpoint"] {
        assert!(
            !src.contains(forbidden),
            "chip-core must not persist ({forbidden})"
        );
    }
}
