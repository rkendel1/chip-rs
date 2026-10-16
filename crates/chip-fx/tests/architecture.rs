//! Dependency direction: Consumer -> Chip -> FX API -> authoritative FX core.
//! Chip's loop and model port know nothing of FX or HTTP; only the two adapter crates
//! reference FX; reqwest lives only in the legacy executor.

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

const FX_CRATES: [&str; 5] = [
    "fx-core",
    "fx-single-attempt",
    "fx-wasm-abi",
    "fx-host",
    "fx-native-provider",
];

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
        for forbidden in FX_CRATES
            .iter()
            .chain(&["chip-fx", "chip-legacy-http", "reqwest"])
        {
            assert!(
                !deps.contains(&forbidden.to_string()),
                "{name} must not depend on {forbidden}"
            );
        }
    }
}

#[test]
fn only_the_adapter_crates_reference_fx() {
    for name in all_crates() {
        let deps = dependency_names(&manifest(&name));
        let uses_fx = FX_CRATES.iter().any(|f| deps.contains(&f.to_string()));
        let allowed = name == "chip-fx" || name == "chip-wasm-host" || name == "chip-fx-native";
        assert!(
            !uses_fx || allowed,
            "{name} references FX but is not an adapter crate"
        );
    }
}

#[test]
fn chip_fx_reaches_providers_only_through_fx() {
    let deps = dependency_names(&manifest("chip-fx"));
    for forbidden in ["reqwest", "hyper", "ureq", "chip-legacy-http", "chip-cli"] {
        assert!(
            !deps.contains(&forbidden.to_string()),
            "chip-fx must not depend on {forbidden}"
        );
    }
    assert!(deps.contains(&"fx-core".to_string()));
    assert!(deps.contains(&"chip-inference".to_string()));
}

/// The native transport (ureq, rustls) links only into the native adapter crate, so the crates
/// that compile for `wasm32` never see it.
#[test]
fn the_native_provider_is_reached_only_through_chip_fx_native() {
    for name in all_crates() {
        let deps = dependency_names(&manifest(&name));
        if deps.contains(&"fx-native-provider".to_string()) {
            assert_eq!(
                name, "chip-fx-native",
                "{name} must not link the native provider"
            );
        }
        if deps.contains(&"chip-fx-native".to_string()) {
            assert!(
                !["chip-core", "chip-inference", "chip-fx", "chip-wasm-host"]
                    .contains(&name.as_str()),
                "{name} compiles for wasm32 and must not depend on chip-fx-native"
            );
        }
    }
}

#[test]
fn chip_fx_native_adds_no_second_executor_or_http_client_of_its_own() {
    let deps = dependency_names(&manifest("chip-fx-native"));
    for forbidden in ["reqwest", "hyper", "ureq", "chip-legacy-http", "chip-cli"] {
        assert!(
            !deps.contains(&forbidden.to_string()),
            "chip-fx-native must not depend on {forbidden}"
        );
    }
    for required in [
        "chip-fx",
        "fx-core",
        "fx-single-attempt",
        "fx-native-provider",
    ] {
        assert!(deps.contains(&required.to_string()), "missing {required}");
    }
}

#[test]
fn no_crate_in_this_workspace_has_an_http_client_of_its_own() {
    // The only HTTP transport is FX's native provider (an fx dependency). The legacy executor that
    // used `reqwest` here was removed.
    for name in all_crates() {
        let deps = dependency_names(&manifest(&name));
        for client in ["reqwest", "hyper", "ureq"] {
            assert!(
                !deps.contains(&client.to_string()),
                "{name} must not use {client}"
            );
        }
    }
}

#[test]
fn no_second_fx_core_exists_in_this_workspace() {
    let crates = all_crates();
    for name in ["fx-core", "fx-provider-http", "fx-single-attempt"] {
        assert!(
            !crates.contains(&name.to_string()),
            "{name} is the authoritative FX's name; no copy here"
        );
    }
}
