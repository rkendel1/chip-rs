use std::fs;
use std::path::{Path, PathBuf};

use chip_graph::{
    ArchitectureGraph, CapabilityCatalog, CatalogError, ImpactReport, analyze, analyze_impact,
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/impact_fixture")
}

fn graph() -> ArchitectureGraph {
    analyze(&fixture()).unwrap()
}

fn catalog_text() -> String {
    fs::read_to_string(fixture().join("capabilities.json")).unwrap()
}

fn catalog(graph: &ArchitectureGraph) -> CapabilityCatalog {
    CapabilityCatalog::from_json(&catalog_text(), graph).unwrap()
}

fn impact(paths: &[&str]) -> ImpactReport {
    let g = graph();
    let c = catalog(&g);
    let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
    analyze_impact(&g, &c, &paths)
}

fn ids(report: &ImpactReport) -> Vec<&str> {
    report
        .impacted_capabilities
        .iter()
        .map(|c| c.id.as_str())
        .collect()
}

fn invalid(json: &str) -> CatalogError {
    CapabilityCatalog::from_json(json, &graph()).unwrap_err()
}

#[test]
fn direct_impact_reaches_file_module_and_crate_bindings() {
    let report = impact(&["a/src/one.rs"]);
    assert_eq!(ids(&report), ["cap.a", "cap.a.file", "cap.shared"]);
    assert_eq!(
        report.changed_nodes,
        ["crate:a", "file:a/src/one.rs", "module:a:crate::one"]
    );
    assert!(report.unresolved_paths.is_empty());
}

#[test]
fn a_shared_capability_is_reported_once_with_only_what_matched() {
    let report = impact(&["a/src/one.rs"]);
    let shared: Vec<_> = report
        .impacted_capabilities
        .iter()
        .filter(|c| c.id == "cap.shared")
        .collect();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].matched_nodes, ["crate:a"]);

    let both = impact(&["a/src/one.rs", "b/src/one.rs"]);
    let shared = both
        .impacted_capabilities
        .iter()
        .find(|c| c.id == "cap.shared")
        .unwrap();
    assert_eq!(shared.matched_nodes, ["crate:a", "crate:b"]);
}

#[test]
fn an_unrelated_change_does_not_impact_other_capabilities() {
    let report = impact(&["b/src/one.rs"]);
    assert_eq!(ids(&report), ["cap.b", "cap.shared"]);
    assert!(!ids(&report).contains(&"cap.a"));
}

#[test]
fn duplicate_and_prefixed_paths_equal_one_path() {
    let one = impact(&["a/src/one.rs"]);
    assert_eq!(impact(&["file:a/src/one.rs", "file:a/src/one.rs"]), one);
    assert_eq!(impact(&["a/src/one.rs", "./a/src/one.rs"]), one);
}

#[test]
fn input_order_never_changes_the_output_bytes() {
    let x = impact(&[
        "b/src/one.rs",
        "missing/z.rs",
        "a/src/one.rs",
        "missing/a.rs",
    ]);
    let y = impact(&[
        "missing/a.rs",
        "a/src/one.rs",
        "missing/z.rs",
        "b/src/one.rs",
    ]);
    assert_eq!(x.render(), y.render());
    assert_eq!(x, y);
    assert_eq!(x.unresolved_paths, ["missing/a.rs", "missing/z.rs"]);
}

#[test]
fn unresolved_paths_are_reported_not_ignored() {
    let report = impact(&["does/not/exist.rs"]);
    assert_eq!(report.unresolved_paths, ["does/not/exist.rs"]);
    assert!(report.impacted_capabilities.is_empty());
    assert!(
        report
            .render()
            .contains("Unresolved:\n  does/not/exist.rs\n")
    );
}

#[test]
fn the_catalog_is_normalized_deterministically() {
    let c = catalog(&graph());
    let order: Vec<_> = c.capabilities.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(order, ["cap.a", "cap.a.file", "cap.b", "cap.shared"]);
    assert_eq!(c.capabilities[0].graph_nodes, ["crate:a"]);
    assert_eq!(c.capabilities[3].graph_nodes, ["crate:a", "crate:b"]);
}

#[test]
fn invalid_catalogs_are_rejected() {
    assert!(matches!(
        invalid(r#"{"schema":"appport.capabilities.v1"}"#),
        CatalogError::WrongSchema(_)
    ));
    assert!(matches!(
        invalid(r#"{"capabilities":[]}"#),
        CatalogError::WrongSchema(_)
    ));
    assert!(matches!(invalid("not json"), CatalogError::NotJson(_)));
    let wrap =
        |body: &str| format!(r#"{{"schema":"chip.capabilities.v1","capabilities":[{body}]}}"#);
    assert!(matches!(
        invalid(&wrap(
            r#"{"id":"x","graph_nodes":["file:does/not/exist.rs"]}"#
        )),
        CatalogError::UnknownGraphNode { .. }
    ));
    assert!(matches!(
        invalid(&wrap(
            r#"{"id":"x","graph_nodes":["crate:a"]},{"id":"x","graph_nodes":["crate:b"]}"#
        )),
        CatalogError::DuplicateId(_)
    ));
    assert_eq!(
        invalid(&wrap(r#"{"id":"","graph_nodes":["crate:a"]}"#)),
        CatalogError::EmptyId
    );
    assert_eq!(
        invalid(&wrap(r#"{"id":"  ","graph_nodes":["crate:a"]}"#)),
        CatalogError::EmptyId
    );
    assert!(matches!(
        invalid(&wrap(r#"{"id":"x","graph_nodes":[]}"#)),
        CatalogError::EmptyGraphNodes(_)
    ));
    // No descriptions or other free-form metadata.
    assert!(matches!(
        invalid(&wrap(
            r#"{"id":"x","graph_nodes":["crate:a"],"description":"deploys things"}"#
        )),
        CatalogError::Malformed(_)
    ));
}

#[test]
fn a_tampered_snapshot_is_still_refused() {
    let tampered = graph().to_snapshot_json().replace("crate:a", "crate:z");
    assert!(ArchitectureGraph::from_snapshot_json(&tampered).is_err());
}

#[test]
fn the_boundary_invariant_is_documented() {
    let lib = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap();
    assert!(lib.contains(
        "Capability identity may cross the Chip boundary; capability implementation does not."
    ));
}

#[test]
fn chip_graph_knows_nothing_about_any_capability_provider() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut texts = vec![fs::read_to_string(root.join("Cargo.toml")).unwrap()];
    for entry in fs::read_dir(root.join("src")).unwrap() {
        texts.push(fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    for text in texts {
        let lower = text.to_lowercase();
        for banned in [
            "appport",
            "node.js",
            "nodejs",
            "http",
            "reqwest",
            "hyper",
            "tokio",
            "std::process",
            "command::new",
            "chip-core",
            "chip-compute",
            "fx-core",
            "chip-local-ml",
            "chip-laya-reasoner",
            "laya",
            "candle",
            "wasm",
        ] {
            assert!(
                !lower.contains(banned),
                "chip-graph must not mention {banned}"
            );
        }
    }
}
