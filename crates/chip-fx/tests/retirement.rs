//! Retirement ratchet for the legacy HTTP executor. The consumer list may only shrink.

use std::collections::BTreeSet;
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
        } else if name == "Cargo.toml" || name.ends_with(".rs") {
            out.push(p);
        }
    }
}

fn uses_legacy(text: &str) -> bool {
    text.lines().any(|l| {
        let l = l.trim();
        if l.starts_with("//") {
            return false;
        }
        l.contains("chip_legacy_http::")
            || l.starts_with("use chip_legacy_http")
            || (l.starts_with("chip-legacy-http") && l.contains('='))
    })
}

#[test]
fn legacy_http_consumers_equal_the_ratchet_list() {
    let crates = root().join("crates");
    let mut files = Vec::new();
    walk(&crates, &mut files);
    let mut actual = BTreeSet::new();
    for f in files {
        let rel = f
            .strip_prefix(root())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        // The parity test compares the two executors; it is the evidence for retiring the legacy
        // one and is deleted with it. It is not a consumer.
        if rel.starts_with("crates/chip-legacy-http/")
            || rel.starts_with("crates/chip-fx/tests/")
            || rel == "crates/chip-cli/tests/executor_parity.rs"
        {
            continue;
        }
        if uses_legacy(&fs::read_to_string(&f).unwrap()) {
            actual.insert(rel);
        }
    }
    let listed: BTreeSet<String> =
        fs::read_to_string(root().join("docs/product/legacy-http-consumers.txt"))
            .unwrap()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect();
    let new: Vec<_> = actual.difference(&listed).collect();
    let stale: Vec<_> = listed.difference(&actual).collect();
    assert!(
        new.is_empty(),
        "new use of the legacy executor (use chip-fx): {new:?}"
    );
    assert!(
        stale.is_empty(),
        "stale ratchet entries (remove them): {stale:?}"
    );
}

/// Production code builds its model through `provider_selection`, whose default is FX. Only that
/// file may construct the legacy provider (test modules and the legacy crate itself excepted), so
/// a new consumer cannot quietly depend on `chip-legacy-http`.
#[test]
fn only_provider_selection_constructs_the_legacy_provider_in_production_code() {
    let mut files = Vec::new();
    walk(&root().join("crates"), &mut files);
    let mut offenders = Vec::new();
    for f in files {
        let rel = f
            .strip_prefix(root())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let production = rel.ends_with(".rs")
            && !rel.starts_with("crates/chip-legacy-http/")
            && !rel.contains("/tests/")
            && rel != "crates/chip-cli/src/provider_selection.rs";
        if !production {
            continue;
        }
        let text = fs::read_to_string(&f).unwrap();
        let before_tests = text.split("#[cfg(test)]").next().unwrap_or_default();
        if before_tests
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .any(|l| l.contains("HttpProvider::new("))
        {
            offenders.push(rel);
        }
    }
    assert!(
        offenders.is_empty(),
        "build the model with provider_selection::build_model_from_process (or build_model); \
         these construct the legacy provider directly: {offenders:?}"
    );
}
