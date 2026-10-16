//! `chip-legacy-http` (the second inference executor) is gone, and stays gone.
//!
//! FX is the only executor. This test fails if the crate comes back, if anything depends on it or
//! imports it, if a script or workflow names it, or if anything outside the shared selection boundary
//! constructs a model provider from a vendor HTTP client.

use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            if name != "target" && !name.starts_with('.') {
                walk(&p, out);
            }
        } else {
            out.push(p);
        }
    }
}

fn code_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter(|l| !l.trim_start().starts_with("//"))
}

#[test]
fn the_legacy_crate_is_gone() {
    assert!(!root().join("crates/chip-legacy-http").exists());
}

#[test]
fn nothing_depends_on_or_imports_the_legacy_crate() {
    let mut files = Vec::new();
    walk(&root().join("crates"), &mut files);
    files.push(root().join("Cargo.toml"));
    let mut offenders = Vec::new();
    for f in files {
        let rel = f
            .strip_prefix(root())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        // This file names the crate in order to forbid it.
        if rel == "crates/chip-fx/tests/retirement.rs" {
            continue;
        }
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        if !(name == "Cargo.toml" || name.ends_with(".rs")) {
            continue;
        }
        let text = fs::read_to_string(&f).unwrap_or_default();
        let hit = code_lines(&text).any(|l| {
            let l = l.trim();
            l.contains("chip_legacy_http::")
                || l.starts_with("use chip_legacy_http")
                || l.contains("HttpProvider::new(")
                || (name == "Cargo.toml" && l.contains("chip-legacy-http") && l.contains('='))
                || (name == "Cargo.toml" && l.contains("crates/chip-legacy-http"))
        });
        if hit {
            offenders.push(rel);
        }
    }
    assert!(
        offenders.is_empty(),
        "chip-legacy-http was removed; FX is the only executor: {offenders:?}"
    );
}

#[test]
fn scripts_and_workflows_do_not_name_the_removed_crate() {
    let mut files = Vec::new();
    walk(&root().join("scripts"), &mut files);
    walk(&root().join(".github"), &mut files);
    let offenders: Vec<_> = files
        .iter()
        .filter(|f| {
            fs::read_to_string(f)
                .unwrap_or_default()
                .contains("chip-legacy-http")
        })
        .map(|f| f.strip_prefix(root()).unwrap().display().to_string())
        .collect();
    assert!(offenders.is_empty(), "{offenders:?}");
}

/// The executor choice is one place. Production code builds its model through
/// `provider_selection`, never from an HTTP client of its own.
#[test]
fn production_code_has_no_http_client_of_its_own() {
    let mut files = Vec::new();
    walk(&root().join("crates"), &mut files);
    let mut offenders = Vec::new();
    for f in files {
        let rel = f
            .strip_prefix(root())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if !rel.ends_with(".rs") || rel.contains("/tests/") || rel.contains("/benches/") {
            continue;
        }
        let text = fs::read_to_string(&f).unwrap();
        let before_tests = text.split("#[cfg(test)]").next().unwrap_or_default();
        if code_lines(before_tests).any(|l| l.contains("reqwest::") || l.contains("hyper::")) {
            offenders.push(rel);
        }
    }
    assert!(offenders.is_empty(), "{offenders:?}");
}
