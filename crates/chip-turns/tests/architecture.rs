//! chip-turns is the one place Chip's loop meets the session store. It must not grow a provider
//! path, a database, or an FX dependency: the model comes in through chip-core's Agent and every
//! durable write goes through chip-session.

use std::fs;
use std::path::Path;

fn deps(manifest: &str, section: &str) -> Vec<String> {
    let mut active = false;
    let mut names = Vec::new();
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            active = line == format!("[{section}]");
        } else if active && let Some((name, _)) = line.split_once('=') {
            names.push(name.trim().to_string());
        }
    }
    names
}

#[test]
fn normal_dependencies_are_exactly_chip_core_chip_session_and_serde_json() {
    let manifest =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    let mut normal = deps(&manifest, "dependencies");
    normal.sort();
    assert_eq!(
        normal,
        ["chip-core", "chip-session", "serde_json"],
        "a new dependency is a new authority: justify it in docs/product/session-format.md"
    );
}

#[test]
fn no_crate_but_chip_turns_depends_on_both_the_loop_and_the_store() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    for entry in fs::read_dir(&crates).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(manifest) = fs::read_to_string(entry.path().join("Cargo.toml")) else {
            continue;
        };
        let all: Vec<String> = ["dependencies", "dev-dependencies"]
            .iter()
            .flat_map(|s| deps(&manifest, s))
            .collect();
        let loop_dep = all.contains(&"chip-core".to_string()) || name == "chip-core";
        let store_dep = all.contains(&"chip-session".to_string());
        if store_dep && loop_dep {
            assert_eq!(
                name, "chip-turns",
                "{name} links the loop and the session store; only chip-turns may"
            );
        }
    }
}
