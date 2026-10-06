//! Test enforcing the dependency direction:
//! chip-core MAY depend on fx-core
//! fx-core MUST NOT depend on chip-core

use std::fs;
use std::path::PathBuf;

fn get_workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn fx_core_does_not_depend_on_chip_core() {
    let fx_manifest_path = get_workspace_root().join("crates/fx-core/Cargo.toml");
    let manifest = fs::read_to_string(fx_manifest_path).expect("fx-core Cargo.toml must exist");

    assert!(
        !manifest.contains("chip-core"),
        "fx-core must not depend on chip-core"
    );
}

#[test]
fn chip_core_depends_on_fx_core() {
    let chip_manifest_path = get_workspace_root().join("crates/chip-core/Cargo.toml");
    let manifest = fs::read_to_string(chip_manifest_path).expect("chip-core Cargo.toml must exist");

    assert!(
        manifest.contains("fx-core"),
        "chip-core must depend on fx-core"
    );
}

#[test]
fn workspace_resolver_is_configured() {
    let workspace_manifest_path = get_workspace_root().join("Cargo.toml");
    let manifest =
        fs::read_to_string(workspace_manifest_path).expect("workspace Cargo.toml must exist");

    assert!(
        manifest.contains("resolver = \"2\""),
        "workspace must use resolver version 2"
    );
}
