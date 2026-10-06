//! PR8: the observer only consumes an ExecutionResult and returns an Observation.

use std::fs;
use std::path::PathBuf;

fn source() -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/observation.rs"))
        .unwrap()
        .to_lowercase()
}

#[test]
fn observation_module_references_no_runtime_or_system_apis() {
    let src = source();
    for forbidden in [
        "compute",
        "appport",
        "boundry",
        "feltdb",
        "attn",
        "pax",
        "http",
        "shell",
        "std::process",
        "std::fs",
        "std::net",
        "tokio",
        "async",
        "await",
        "reqwest",
        "openai",
        "anthropic",
        "fx_core",
        "fx_provider",
        "modelprovider",
        "executor",
        "chip_compute",
        "sqlite",
        "command",
    ] {
        assert!(
            !src.contains(forbidden),
            "observation.rs must not reference {forbidden}"
        );
    }
}

#[test]
fn observation_kinds_are_generic_only() {
    let src = source();
    let start = src.find("pub enum observationkind").unwrap();
    let body = &src[start..src[start..].find('}').unwrap() + start];
    for domain in ["deploy", "test", "file", "browser", "build"] {
        assert!(
            !body.contains(domain),
            "ObservationKind must stay generic ({domain})"
        );
    }
}

#[test]
fn observer_is_synchronous_and_the_agent_does_not_observe_inside_run_turn() {
    let src = source();
    assert!(src.contains("fn observe(&self, result: &executionresult)"));
    let lib =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap();
    let start = lib.find("pub async fn run_turn(").unwrap();
    let body = &lib[start..lib[start..].find("\n    }\n").unwrap() + start];
    assert!(
        !body.contains("observe"),
        "run_turn must not observe or feed back"
    );
}

#[test]
fn no_persistence_or_adapter_dependencies() {
    let manifest =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    for forbidden in ["chip-compute", "fx-provider-http", "sqlite", "feltdb"] {
        assert!(!manifest.to_lowercase().contains(forbidden));
    }
}
