//! PR3: chip-core owns the Executor trait and knows no concrete execution system.

use std::fs;
use std::path::PathBuf;

fn chip_core() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

const FORBIDDEN: [&str; 6] = [
    "compute",
    "compute-configured",
    "appport",
    "feltdb",
    "attn",
    "pax",
];

fn norm(s: &str) -> String {
    s.to_lowercase().replace('_', "-")
}

#[test]
fn chip_core_manifest_has_no_execution_system_dependency() {
    let manifest = fs::read_to_string(chip_core().join("Cargo.toml")).unwrap();
    for line in manifest.lines().map(norm) {
        let name = line.split('=').next().unwrap().trim().to_string();
        for forbidden in FORBIDDEN {
            assert!(
                !(line.contains('=') && name.contains(forbidden)),
                "chip-core must not depend on {forbidden}: {line}"
            );
        }
    }
}

#[test]
fn chip_core_source_does_not_reference_execution_systems() {
    for file in ["src/lib.rs", "src/decision.rs", "tests/execution.rs"] {
        let source = norm(&fs::read_to_string(chip_core().join(file)).unwrap());
        for forbidden in FORBIDDEN {
            assert!(!source.contains(forbidden), "{file} references {forbidden}");
        }
    }
}

#[test]
fn chip_core_owns_the_executor_trait() {
    let source = fs::read_to_string(chip_core().join("src/lib.rs")).unwrap();
    assert!(source.contains("pub trait Executor"));
}

#[test]
fn only_chip_compute_is_an_execution_adapter() {
    let crates = chip_core().parent().unwrap().to_path_buf();
    for entry in fs::read_dir(crates).unwrap() {
        let name = norm(&entry.unwrap().file_name().to_string_lossy());
        assert!(
            !name.contains("compute") || name == "chip-compute",
            "unexpected adapter crate {name}"
        );
    }
}
