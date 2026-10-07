//! Isolation for the experimental Laya adapter. Runs in the default build (no `laya`
//! feature), so it needs neither candle nor a model.

use std::fs;
use std::path::PathBuf;

fn crates_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn manifest(krate: &str) -> String {
    fs::read_to_string(crates_dir().join(krate).join("Cargo.toml"))
        .unwrap()
        .to_lowercase()
}

fn code(krate: &str, file: &str) -> String {
    fs::read_to_string(crates_dir().join(krate).join(file))
        .unwrap()
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn chip_core_and_the_other_crates_know_nothing_about_laya_or_candle() {
    for krate in [
        "chip-core",
        "fx-core",
        "fx-provider-http",
        "chip-compute",
        "chip-wasm-reasoner",
        "chip-reasoning-corpus",
        "chip-local-ml",
    ] {
        let m = manifest(krate);
        for forbidden in [
            "laya",
            "candle",
            "tokenizers",
            "hf-hub",
            "chip-laya-reasoner",
        ] {
            // chip-local-ml is the other experimental adapter and may mention the Laya *model*
            // by name only through rust-ml-runtime; it must not depend on this crate.
            if krate == "chip-local-ml" && forbidden == "laya" {
                continue;
            }
            assert!(
                !m.contains(forbidden),
                "{krate} must not mention {forbidden}"
            );
        }
    }
}

#[test]
fn the_adapter_does_not_use_rust_ml_runtime_or_other_runtimes() {
    let m = manifest("chip-laya-reasoner");
    for forbidden in [
        "rust-ml-runtime",
        "ml-runtime",
        "onnx",
        "ort",
        "wasmi",
        "reqwest",
        "hyper",
        "tokio",
        "chip-compute",
        "fx-provider",
        "chip-local-ml",
    ] {
        assert!(
            !m.contains(&format!("{forbidden} =")) && !m.contains(&format!("\"{forbidden}\"")),
            "must not depend on {forbidden}"
        );
    }
}

#[test]
fn the_heavy_dependencies_are_opt_in_so_the_default_build_stays_light() {
    let m = manifest("chip-laya-reasoner");
    assert!(m.contains("laya-decision = { version = \"0.2.4\", optional = true }"));
    assert!(m.contains("laya = [\"dep:laya-decision\""));
    let cli = manifest("chip-cli");
    assert!(cli.contains("laya = [\"dep:chip-laya-reasoner\", \"chip-laya-reasoner/laya\"]"));
    assert!(
        cli.contains("chip-laya-reasoner = { path = \"../chip-laya-reasoner\", optional = true }")
    );
}

#[test]
fn the_reasoner_implementation_reaches_nothing_but_explicit_local_model_loading() {
    let src = code("chip-laya-reasoner", "src/lib.rs");
    for forbidden in [
        "std::process",
        "Command",
        "reqwest",
        "hyper",
        "tokio",
        "std::env",
        "env::var",
        "std::net",
        "std::fs",
        "read_to_string",
        "read_dir",
        "File::",
        "chip_compute",
        "fx_provider",
        "fx_core",
        "Executor",
        "ModelProvider",
        "hf_hub",
        "download",
        "token: Some",
    ] {
        assert!(
            !src.contains(forbidden),
            "the adapter must not reference {forbidden}"
        );
    }
    // Loading is explicit and local: only an existing directory is ever handed to Laya.
    assert!(src.contains("canonicalize()"));
    assert!(src.contains("token: None"), "no Hub token is ever passed");
}

#[test]
fn there_are_no_thresholds_or_authority_in_the_adapter() {
    let src = code("chip-laya-reasoner", "src/lib.rs").to_lowercase();
    for forbidden in [
        "threshold",
        "retry",
        "loop {",
        "while ",
        "execute",
        "record_evidence",
        "lookup_evidence",
        "evidencestore",
    ] {
        assert!(
            !src.contains(forbidden),
            "the adapter must not contain {forbidden}"
        );
    }
}

#[test]
fn the_pr17_adapter_is_untouched() {
    // This is a competing experimental adapter; chip-local-ml keeps its own manifest shape.
    let m = manifest("chip-local-ml");
    assert!(m.contains("rust-ml-runtime = { version = \"0.2.3\", optional = true }"));
}
