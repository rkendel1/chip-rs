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
fn fx_core_does_not_depend_on_chip_core_or_provider() {
    let deps = dependency_names(&manifest("fx-core"));
    assert!(!deps.contains(&"chip-core".into()));
    assert!(!deps.contains(&"fx-provider-http".into()));
}

#[test]
fn fx_provider_http_depends_on_fx_core_but_not_chip_core() {
    let deps = dependency_names(&manifest("fx-provider-http"));
    assert!(deps.contains(&"fx-core".into()));
    for forbidden in ["chip-core", "chip-cli"] {
        assert!(
            !deps.contains(&forbidden.to_string()),
            "fx-provider-http must not depend on {forbidden}"
        );
    }
}

#[test]
fn fx_provider_http_has_no_vendor_sdk() {
    let deps = dependency_names(&manifest("fx-provider-http"));
    for sdk in ["openai", "async-openai", "anthropic", "ollama"] {
        assert!(
            !deps.contains(&sdk.to_string()),
            "vendor SDK {sdk} is forbidden"
        );
    }
}

#[test]
fn chip_core_does_not_depend_on_fx_provider_http() {
    let manifest = manifest("chip-core");
    assert!(
        !dependency_names(&manifest).contains(&"fx-provider-http".into()),
        "chip-core must not depend on fx-provider-http (not even as a dev-dependency)"
    );
    for file in ["src/lib.rs", "tests/integration.rs"] {
        let source = fs::read_to_string(crates_dir().join("chip-core").join(file)).unwrap();
        assert!(
            !source.contains("fx_provider_http"),
            "{file} references the HTTP provider"
        );
    }
}
