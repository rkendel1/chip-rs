use std::fs;
use std::path::Path;

#[test]
fn fx_core_has_no_chip_core_dependency() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../fx-core/Cargo.toml"))
        .expect("fx-core Cargo.toml should exist");

    assert!(
        !manifest.contains("chip-core"),
        "fx-core must not depend on chip-core"
    );
}

#[test]
fn chip_core_depends_on_fx_core() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("chip-core Cargo.toml should exist");

    assert!(
        manifest.contains("fx-core"),
        "chip-core must depend on fx-core"
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
