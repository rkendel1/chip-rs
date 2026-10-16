//! `chip-session` reads the Zig-owned store. It is a leaf: no Chip loop, FX, provider or HTTP
//! dependency, and nothing else in the workspace may write a second session format by depending on a
//! database crate through it.

use std::fs;
use std::path::Path;

fn deps(manifest: &str) -> Vec<String> {
    let mut in_deps = false;
    let mut names = Vec::new();
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_deps = line.contains("dependencies");
        } else if in_deps && let Some((name, _)) = line.split_once('=') {
            names.push(name.trim().to_string());
        }
    }
    names
}

#[test]
fn chip_session_is_a_leaf_with_no_chip_fx_provider_or_database_dependency() {
    let manifest =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    for name in deps(&manifest) {
        assert!(
            ["serde", "serde_json", "sha2", "tempfile"].contains(&name.as_str()),
            "unexpected dependency {name}: persistence stays the format's own, not a new store"
        );
    }
}
