//! PR11: the evidence path is local, in-memory and bounded.

use std::fs;
use std::path::PathBuf;

fn read(file: &str) -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(file)).unwrap()
}

#[test]
fn evidence_module_is_in_memory_and_system_free() {
    // Code only: skip the module's own doc comments.
    let code: String = read("src/evidence.rs")
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
        "tokio",
        "http",
        "sqlite",
        "async",
        "static mut",
        "lazy",
        "once",
        "loop",
        "while ",
        "retry",
        "wasm",
        "embedding",
        "executor",
        "modelprovider",
    ] {
        assert!(
            !code.contains(forbidden),
            "evidence.rs must not reference {forbidden}"
        );
    }
}

#[test]
fn obtain_evidence_has_one_execution_site_and_no_model_call() {
    let lib = read("src/lib.rs");
    let start = lib.find("pub async fn obtain_evidence(").unwrap();
    let body = lib[start..start + lib[start..].find("\n    }\n").unwrap()].to_lowercase();
    assert_eq!(body.matches("self.execute(").count(), 1);
    for forbidden in [
        "complete(",
        "model_turn",
        "self.turn",
        "decide",
        "loop",
        "while ",
        "retry",
    ] {
        assert!(
            !body.contains(forbidden),
            "obtain_evidence must not contain {forbidden}"
        );
    }
    assert!(
        body.find("lookup_evidence").unwrap() < body.find("self.execute(").unwrap(),
        "lookup precedes execution"
    );
}

#[test]
fn chip_core_manifest_stays_neutral() {
    let manifest = read("Cargo.toml").to_lowercase();
    for forbidden in [
        "chip-compute",
        "fx-provider-http",
        "sqlite",
        "feltdb",
        "wasm",
        "lru",
        "moka",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "chip-core must not depend on {forbidden}"
        );
    }
}
