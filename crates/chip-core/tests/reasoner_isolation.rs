//! PR13: the reasoning boundary is data in, verdict out, with no reach.

use std::fs;
use std::path::PathBuf;

fn read(file: &str) -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(file)).unwrap()
}

#[test]
fn reasoning_module_has_no_system_executor_or_model_reach() {
    let code: String = read("src/reasoning.rs")
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    for forbidden in [
        "compute",
        "appport",
        "boundry",
        "feltdb",
        "attn",
        "pax",
        "std::fs",
        "std::net",
        "std::process",
        "std::env",
        "tokio",
        "http",
        "async",
        "static mut",
        "lazy",
        "wasm",
        "executor",
        "modelprovider",
        "agent",
        "evidencestore",
        "observation",
        "loop",
        "while ",
    ] {
        assert!(
            !code.contains(forbidden),
            "reasoning.rs must not reference {forbidden}"
        );
    }
}

#[test]
fn the_reasoner_signature_is_structured_and_takes_nothing_else() {
    let src = read("src/reasoning.rs");
    assert!(src.contains(
        "fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError>;"
    ));
    assert!(!src.contains("prompt"), "no prompt-string protocol");
    assert!(!src.contains("-> String"), "no free-form string verdicts");
}

#[test]
fn assess_evidence_only_advises() {
    let lib = read("src/lib.rs");
    let start = lib.find("pub fn assess_evidence(").unwrap();
    let body = lib[start..start + lib[start..].find("\n    }\n").unwrap()].to_lowercase();
    for forbidden in [
        ".await",
        "execute",
        "complete(",
        "record_",
        "obtain_",
        "invalidate",
        "observe(",
        "loop",
        "while ",
    ] {
        assert!(
            !body.contains(forbidden),
            "assess_evidence must not contain {forbidden}"
        );
    }
    assert!(
        body.find("lookup").unwrap() < body.find("reasoner").unwrap(),
        "evidence is consulted first"
    );
}

#[test]
fn chip_core_manifest_stays_free_of_runtimes_and_adapters() {
    let manifest = read("Cargo.toml").to_lowercase();
    for forbidden in [
        "wasmi",
        "wasmtime",
        "chip-wasm",
        "chip-compute",
        "fx-provider-http",
        "sqlite",
        "feltdb",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "chip-core must not depend on {forbidden}"
        );
    }
}
