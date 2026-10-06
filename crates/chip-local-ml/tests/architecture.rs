//! Dependency isolation for the native local-model adapter. These tests run in the
//! default build (no `runtime` feature, so no ONNX Runtime needed).

use std::fs;
use std::path::PathBuf;

fn manifest(krate: &str) -> String {
    fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join(krate)
            .join("Cargo.toml"),
    )
    .unwrap()
    .to_lowercase()
}

fn dependencies(manifest: &str) -> Vec<String> {
    let mut in_deps = false;
    let mut names = Vec::new();
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_deps = line == "[dependencies]";
        } else if in_deps {
            if let Some((name, _)) = line.split_once('=') {
                names.push(name.trim().to_string());
            }
        }
    }
    names
}

#[test]
fn chip_core_and_the_other_crates_do_not_depend_on_the_runtime() {
    for krate in [
        "chip-core",
        "fx-core",
        "fx-provider-http",
        "chip-compute",
        "chip-wasm-reasoner",
        "chip-reasoning-corpus",
    ] {
        let m = manifest(krate);
        for forbidden in [
            "rust-ml-runtime",
            "ml-runtime",
            "onnx",
            "coreml",
            "chip-local-ml",
            "laya",
        ] {
            assert!(
                !m.contains(forbidden),
                "{krate} must not mention {forbidden}"
            );
        }
    }
}

#[test]
fn the_adapter_depends_only_on_chip_core_the_runtime_and_json() {
    assert_eq!(
        dependencies(&manifest("chip-local-ml")),
        ["chip-core", "rust-ml-runtime", "serde_json"]
    );
}

#[test]
fn the_runtime_is_opt_in_so_the_default_build_needs_no_onnx_runtime() {
    let m = manifest("chip-local-ml");
    assert!(m.contains("rust-ml-runtime = { version = \"0.2.3\", optional = true }"));
    assert!(m.contains("runtime = [\"dep:rust-ml-runtime\", \"dep:serde_json\"]"));
    let cli = manifest("chip-cli");
    assert!(cli.contains("local-ml = [\"dep:chip-local-ml\", \"chip-local-ml/runtime\"]"));
    assert!(cli.contains("chip-local-ml = { path = \"../chip-local-ml\", optional = true }"));
}
