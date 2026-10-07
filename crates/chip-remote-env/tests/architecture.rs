//! Product boundaries. Rust Chip + Rust FX are one stack; Compute is an environment some other
//! program provides to it. Nothing in the agent, its model boundary, its capability transport or
//! its production work/serve path may know Compute, and neither stack may use the other's FX.

use std::fs;
use std::path::{Path, PathBuf};

fn crates_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path.as_ref()).unwrap_or_else(|e| panic!("{}: {e}", path.as_ref().display()))
}

fn manifest(name: &str) -> String {
    read(crates_dir().join(name).join("Cargo.toml"))
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

fn sources(name: &str) -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push((path.display().to_string(), read(&path)));
            }
        }
    }
    let mut out = Vec::new();
    walk(&crates_dir().join(name).join("src"), &mut out);
    out
}

/// The crates that make up Rust Chip, Rust FX and the generic capability/environment machinery.
const PRODUCT: &[&str] = &[
    "chip-core",
    "chip-remote-env",
    "fx-core",
    "fx-provider-http",
    "chip-project",
    "chip-pax",
];

#[test]
fn no_product_crate_depends_on_compute_or_its_neighbours() {
    for name in PRODUCT {
        let deps = dependency_names(&manifest(name));
        for dep in &deps {
            let lowered = dep.to_ascii_lowercase();
            for forbidden in ["compute", "feltdb", "appport"] {
                assert!(!lowered.contains(forbidden), "{name} depends on {dep}");
            }
        }
    }
}

#[test]
fn the_rust_fx_boundary_stays_independent_of_chip_compute_and_the_npm_stack() {
    for name in ["fx-core", "fx-provider-http"] {
        let m = manifest(name);
        let deps = dependency_names(&m);
        assert!(
            !deps.contains(&"chip-core".to_string()),
            "{name} must not depend on chip-core"
        );
        for forbidden in ["zig", "npm", "node", "eve"] {
            assert!(
                !deps
                    .iter()
                    .any(|d| d.to_ascii_lowercase().contains(forbidden)),
                "{name} must not depend on the {forbidden} stack"
            );
        }
    }
}

#[test]
fn product_sources_name_no_compute_concept() {
    // The generic crates describe only "an external execution environment". Compute's own nouns
    // belong to Compute's side of the seam.
    let nouns = [
        "ComputeSession",
        "ComputeJob",
        "ComputeReceipt",
        "ComputeEnvironment",
        "ComputeClient",
        "COMPUTE_",
    ];
    for name in PRODUCT {
        for (file, text) in sources(name) {
            for noun in nouns {
                assert!(!text.contains(noun), "{file} mentions {noun}");
            }
        }
    }
    // The transport crate names no product at all.
    for (file, text) in sources("chip-remote-env") {
        let lowered = text.to_ascii_lowercase();
        for word in ["compute", "feltdb", "appport", "npm"] {
            assert!(!lowered.contains(word), "{file} mentions {word}");
        }
    }
}

#[test]
fn the_production_work_and_serve_path_has_no_compute_dependency() {
    let allowed_demo_files = ["horizon.rs", "main.rs", "work_demo.rs"];
    for (file, text) in sources("chip-cli") {
        let name = Path::new(&file)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if text.contains("chip_compute") {
            assert!(
                allowed_demo_files.contains(&name.as_str()),
                "{file} uses chip_compute outside the demo/benchmark path"
            );
        }
    }
    // The runtime service, the scheduler, the work loop and the local environment never do.
    for module in [
        "service.rs",
        "software_work.rs",
        "local_environment.rs",
        "provider_selection.rs",
    ] {
        let text = read(crates_dir().join("chip-cli/src").join(module));
        assert!(!text.contains("chip_compute"), "{module}");
        assert!(
            !text.to_ascii_lowercase().contains("compute-configured"),
            "{module}"
        );
    }
}

#[test]
fn chip_is_the_one_rust_executable() {
    let m = manifest("chip-cli");
    assert_eq!(m.matches("[[bin]]").count(), 1, "exactly one binary target");
    assert!(m.contains("name = \"chip\""), "the binary is `chip`");
    assert!(
        !crates_dir().join("chip-cli/src/bin").exists(),
        "no second Rust command-line program"
    );
}
