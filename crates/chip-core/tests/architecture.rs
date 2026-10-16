use std::fs;
use std::path::Path;

#[test]
fn chip_inference_has_no_chip_core_dependency() {
    let manifest = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../chip-inference/Cargo.toml"),
    )
    .expect("chip-inference Cargo.toml should exist");

    assert!(
        !manifest.contains("chip-core"),
        "chip-inference must not depend on chip-core"
    );
}

#[test]
fn chip_core_depends_on_chip_inference() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("chip-core Cargo.toml should exist");

    assert!(
        manifest.contains("chip-inference"),
        "chip-core must depend on chip-inference"
    );
}

#[test]
fn chip_core_has_no_forbidden_runtime_dependencies() {
    let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"))
        .expect("chip-core src/lib.rs should exist");

    for forbidden in [
        "Compute",
        "Attn",
        "FeltDB",
        "AppPort",
        "filesystem",
        "shell",
        "terminal",
        "deployment",
        "browser",
    ] {
        assert!(
            !source.contains(forbidden),
            "chip-core should not include {forbidden}"
        );
    }
}
