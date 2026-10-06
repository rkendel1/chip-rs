//! Test ensuring PR1 does not accidentally introduce forbidden runtime dependencies.

use std::fs;
use std::path::PathBuf;

fn get_crate_root(crate_name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("crates/{crate_name}"))
}

fn check_crate_for_forbidden_patterns(crate_name: &str, forbidden_patterns: &[&str]) {
    let manifest_path = get_crate_root(crate_name).join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .expect(&format!("{} Cargo.toml must exist", crate_name));

    for pattern in forbidden_patterns {
        assert!(
            !manifest.contains(pattern),
            "{} should not depend on {}",
            crate_name,
            pattern
        );
    }
}

#[test]
fn chip_core_has_no_forbidden_dependencies() {
    let forbidden = [
        "openai",
        "anthropic",
        "ollama",
        "sqlite",
        "postgres",
        "mongodb",
        "reqwest",
        "hyper",
        "tokio-tungstenite",
    ];
    check_crate_for_forbidden_patterns("chip-core", &forbidden);
}

#[test]
fn fx_core_has_no_forbidden_dependencies() {
    let forbidden = [
        "openai",
        "anthropic",
        "ollama",
        "sqlite",
        "postgres",
        "mongodb",
        "reqwest",
        "hyper",
    ];
    check_crate_for_forbidden_patterns("fx-core", &forbidden);
}

#[test]
fn source_code_has_no_forbidden_keywords() {
    let chip_core_src = get_crate_root("chip-core").join("src/lib.rs");
    let source = fs::read_to_string(chip_core_src)
        .expect("chip-core src/lib.rs must exist");

    let forbidden_keywords = [
        "Compute", "Attn", "FeltDB", "AppPort", "browser", "shell", "terminal", "MCP",
    ];

    for keyword in &forbidden_keywords {
        assert!(
            !source.contains(keyword),
            "chip-core source should not reference {}",
            keyword
        );
    }
}
