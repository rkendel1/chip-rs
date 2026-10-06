//! The WASM host sits outside chip-core and reaches nothing but chip-core.

use std::fs;
use std::path::PathBuf;

fn read(path: &str) -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap()
        .to_lowercase()
}

fn deps(manifest: &str) -> Vec<String> {
    let mut in_deps = false;
    let mut names = Vec::new();
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_deps = line.starts_with("[dependencies");
        } else if in_deps {
            if let Some((name, _)) = line.split_once('=') {
                names.push(name.trim().to_string());
            }
        }
    }
    names
}

#[test]
fn the_host_depends_only_on_chip_core_and_the_wasm_engine() {
    let names = deps(&read("Cargo.toml"));
    assert_eq!(names, ["chip-core", "wasmi", "wat"]);
    // `wat` is only the optional fixture compiler, never part of the reasoner itself.
    assert!(read("Cargo.toml").contains("wat = { version = \"1\", optional = true }"));
}

#[test]
fn the_host_grants_no_host_functions() {
    let src = read("src/lib.rs");
    for forbidden in [
        "wasi",
        "func_wrap",
        "define(",
        "std::fs",
        "std::net",
        "std::process",
        "std::env",
        "tokio",
        "fx_core",
        "chip_compute",
    ] {
        // Allowed only inside the explanatory doc comments.
        let code: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains(forbidden),
            "host must not reference {forbidden}"
        );
    }
}

#[test]
fn chip_core_does_not_depend_on_the_wasm_host_or_engine() {
    let core = read("../chip-core/Cargo.toml");
    assert!(!core.contains("wasmi") && !core.contains("chip-wasm-reasoner"));
}
