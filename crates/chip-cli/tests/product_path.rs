//! Production agency is not implemented by a test double or by research code.
//!
//! `chip work`, `chip verify` and `chip serve` run on the modules below. Their production code (the
//! part before the test module) may name only the crates of the product allowlist, and none of the
//! reasoner types. Experiments and proof commands live elsewhere in `chip-cli` and are allowed to
//! use `TestLocalReasoner`; the product modules are not.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The crates the product path may use (identifiers as written in Rust source).
const PRODUCT_CRATES: &[&str] = &[
    "chip_core",
    "chip_pax",
    "chip_project",
    "chip_remote_env",
    "fx_core",
    "fx_provider_http",
];

/// Types the product path must never name: reasoners, whether test doubles or experiments.
const FORBIDDEN: &[&str] = &[
    "TestLocalReasoner",
    "LocalReasoner",
    "with_local_reasoner",
    "CapabilityDecisionState",
    "LocalReasoningResult",
];

/// The `chip-cli` modules `work`, `verify` and `serve` run on.
const CLI_PRODUCT_MODULES: &[&str] = &[
    "software_work.rs",
    "verify.rs",
    "service.rs",
    "local_environment.rs",
    "provider_selection.rs",
    "micro.rs",
];

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// The file's production code: everything before its `#[cfg(test)] mod tests`.
fn production_part(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    match text.find("#[cfg(test)]\nmod tests") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `chip_*` / `fx_*` crate identifier a source text mentions.
fn crate_identifiers(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        let word = &text[start..i];
        let is_crate = word.starts_with("chip_") || word.starts_with("fx_");
        // A path or `use` mention, not a longer identifier or a string such as "chip_cli".
        if is_crate && text[i..].starts_with("::") {
            found.insert(word.to_string());
        }
    }
    found
}

fn product_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = CLI_PRODUCT_MODULES
        .iter()
        .map(|m| crates_dir().join("chip-cli/src").join(m))
        .collect();
    for krate in ["chip-core", "chip-project", "chip-pax", "chip-remote-env"] {
        rust_files(&crates_dir().join(krate).join("src"), &mut files);
    }
    files
}

#[test]
fn the_product_path_names_no_reasoner() {
    for file in product_files() {
        // `chip-core` defines the seam and the double; everything else only consumes the product.
        if file.starts_with(crates_dir().join("chip-core")) {
            continue;
        }
        let text = production_part(&file);
        for word in FORBIDDEN {
            assert!(
                !text.contains(word),
                "{} names {word}: the product path must not use a reasoner",
                file.display()
            );
        }
    }
}

#[test]
fn the_product_path_depends_only_on_product_crates() {
    for file in product_files() {
        if !file.starts_with(crates_dir().join("chip-cli")) {
            continue;
        }
        for ident in crate_identifiers(&production_part(&file)) {
            assert!(
                PRODUCT_CRATES.contains(&ident.as_str()),
                "{} uses {ident}, which is not a product crate",
                file.display()
            );
        }
    }
}

#[test]
fn the_work_loop_does_not_read_the_local_decision_research_state() {
    // `decision_state.rs` is experimental residue (see its module documentation).
    for module in [
        "work.rs",
        "environment.rs",
        "evidence.rs",
        "observation.rs",
        "capability_set.rs",
        "model_decision.rs",
    ] {
        let text = production_part(&crates_dir().join("chip-core/src").join(module));
        for word in [
            "decision_state",
            "CapabilityDecisionState",
            "GraphStateToken",
            "ImpactState",
        ] {
            assert!(!text.contains(word), "chip-core/src/{module} uses {word}");
        }
    }
}

#[test]
fn the_agent_the_product_builds_has_no_reasoner_installed() {
    // The two places the product constructs its `Agent` install exactly the capabilities, the
    // executor and the observer, and nothing that decides.
    for module in ["software_work.rs", "verify.rs"] {
        let text = production_part(&crates_dir().join("chip-cli/src").join(module));
        assert_eq!(text.matches("Agent::with_model(").count(), 1, "{module}");
        assert!(!text.contains(".with_local_reasoner("), "{module}");
    }
}

/// Modules of `chip-cli` the product path may use. Everything else in `chip-cli` is a proof,
/// benchmark or experiment command.
const PRODUCT_MODULES: &[&str] = &[
    "software_work",
    "verify",
    "service",
    "local_environment",
    "provider_selection",
    "micro",
];

/// Known remaining couplings from the product path to a proof module, listed so they cannot grow:
/// the canonical measurement renderer still lives in `work_demo.rs`.
const KNOWN_PROOF_COUPLINGS: &[&str] = &["work_demo::measurement_json"];

#[test]
fn the_product_path_uses_no_proof_or_experiment_module() {
    for module in CLI_PRODUCT_MODULES {
        let path = crates_dir().join("chip-cli/src").join(module);
        let text = production_part(&path);
        let mut rest = text.as_str();
        while let Some(at) = rest.find("crate::") {
            rest = &rest[at + "crate::".len()..];
            let path_end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
                .unwrap_or(rest.len());
            let used = &rest[..path_end];
            let first = used.split("::").next().unwrap_or("");
            // `crate::config_from_env` and similar are items of the library root, not modules.
            let is_module = !first.is_empty() && !first.starts_with(char::is_uppercase);
            let is_root_item = !used.contains("::");
            if is_module && !is_root_item && !PRODUCT_MODULES.contains(&first) {
                assert!(
                    KNOWN_PROOF_COUPLINGS.contains(&used),
                    "{module} uses crate::{used}: a proof or experiment module"
                );
            }
        }
    }
}
