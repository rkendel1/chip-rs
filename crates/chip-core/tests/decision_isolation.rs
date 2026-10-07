//! PR6: the decision layer is executor-, provider- and Compute-neutral.

use std::fs;
use std::path::PathBuf;

fn source() -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/decision.rs")).unwrap()
}

#[test]
fn decision_module_does_not_reference_executors_or_execution_systems() {
    let src = source().to_lowercase();
    for forbidden in [
        "executor",
        "executionresult",
        "executionreport",
        "compute",
        "chip-compute",
        "appport",
        "boundry",
        "feltdb",
        "attn",
        "pax",
        "tokio",
        "reqwest",
        "std::process",
        "command",
        "openai",
        "anthropic",
    ] {
        assert!(
            !src.contains(forbidden),
            "decision.rs must not reference {forbidden}"
        );
    }
}

#[test]
fn decision_requests_carry_no_implementation_details() {
    let src = source();
    let start = src.find("pub struct CapabilityRequest").unwrap();
    let body = &src[start..src[start..].find('}').unwrap() + start];
    for field in ["runtime", "path", "env", "pid", "container", "operation"] {
        assert!(
            !body.contains(field),
            "CapabilityRequest must not carry {field}"
        );
    }
}

#[test]
fn agent_decision_has_no_execute_variant() {
    let src = source();
    let start = src.find("pub enum AgentDecision").unwrap();
    let body = &src[start..src[start..].find('}').unwrap() + start];
    assert!(!body.to_lowercase().contains("execut"));
}

#[test]
fn chip_core_manifest_stays_neutral() {
    let manifest =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    for forbidden in [
        "chip-compute",
        "fx-provider-http",
        "reqwest",
        "tokio-process",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "chip-core must not depend on {forbidden}"
        );
    }
}
