//! PR2 architecture rules for the FX provider boundary.

use std::fs;
use std::path::PathBuf;

fn crates_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn manifest(name: &str) -> String {
    fs::read_to_string(crates_dir().join(name).join("Cargo.toml"))
        .unwrap_or_else(|_| panic!("{name} Cargo.toml must exist"))
}

fn dependency_names(manifest: &str) -> Vec<String> {
    let mut in_deps = false;
    let mut names = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_deps = line.contains("dependencies");
        } else if in_deps {
            if let Some((name, _)) = line.split_once('=') {
                names.push(name.trim().to_string());
            }
        }
    }
    names
}

#[test]
fn chip_inference_does_not_depend_on_chip_core_or_provider() {
    let deps = dependency_names(&manifest("chip-inference"));
    assert!(!deps.contains(&"chip-core".into()));
    assert!(!deps.contains(&"chip-legacy-http".into()));
}

#[test]
fn chip_core_does_not_depend_on_chip_legacy_http() {
    let manifest = manifest("chip-core");
    assert!(
        !dependency_names(&manifest).contains(&"chip-legacy-http".into()),
        "chip-core must not depend on chip-legacy-http (not even as a dev-dependency)"
    );
    for file in ["src/lib.rs", "tests/integration.rs"] {
        let source = fs::read_to_string(crates_dir().join("chip-core").join(file)).unwrap();
        assert!(
            !source.contains("chip_legacy_http"),
            "{file} references the HTTP provider"
        );
    }
}
