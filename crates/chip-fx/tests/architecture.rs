//! Dependency direction: Consumer -> Chip -> FX API -> authoritative FX core.
//! Chip's loop and model port know nothing of FX or HTTP; only the two adapter crates
//! reference FX; reqwest lives only in the legacy executor.

use std::fs;
use std::path::PathBuf;

fn crates_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
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

const FX_CRATES: [&str; 4] = ["fx-core", "fx-single-attempt", "fx-wasm-abi", "fx-host"];

fn all_crates() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(crates_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("Cargo.toml").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn chip_core_and_model_port_do_not_reference_fx_or_legacy_http() {
    for name in ["chip-core", "chip-inference"] {
        let deps = dependency_names(&manifest(name));
        for forbidden in FX_CRATES.iter().chain(&["chip-fx", "chip-legacy-http", "reqwest"]) {
            assert!(!deps.contains(&forbidden.to_string()), "{name} must not depend on {forbidden}");
        }
    }
}

#[test]
fn only_the_adapter_crates_reference_fx() {
    for name in all_crates() {
        let deps = dependency_names(&manifest(&name));
        let uses_fx = FX_CRATES.iter().any(|f| deps.contains(&f.to_string()));
        let allowed = name == "chip-fx" || name == "chip-wasm-host";
        assert!(!uses_fx || allowed, "{name} references FX but is not an adapter crate");
    }
}

#[test]
fn chip_fx_reaches_providers_only_through_fx() {
    let deps = dependency_names(&manifest("chip-fx"));
    for forbidden in ["reqwest", "hyper", "ureq", "chip-legacy-http", "chip-cli"] {
        assert!(!deps.contains(&forbidden.to_string()), "chip-fx must not depend on {forbidden}");
    }
    assert!(deps.contains(&"fx-core".to_string()));
    assert!(deps.contains(&"chip-inference".to_string()));
}

#[test]
fn reqwest_lives_only_in_the_legacy_executor() {
    for name in all_crates() {
        let deps = dependency_names(&manifest(&name));
        if deps.contains(&"reqwest".to_string()) {
            assert_eq!(name, "chip-legacy-http", "{name} must not use reqwest");
        }
    }
}

#[test]
fn no_second_fx_core_exists_in_this_workspace() {
    let crates = all_crates();
    for name in ["fx-core", "fx-provider-http", "fx-single-attempt"] {
        assert!(!crates.contains(&name.to_string()), "{name} is the authoritative FX's name; no copy here");
    }
}
