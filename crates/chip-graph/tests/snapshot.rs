use std::fs;
use std::path::{Path, PathBuf};

use chip_graph::{ArchitectureGraph, GraphEdgeKind, GraphNodeKind, analyze};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture")
}

fn golden() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/fixture.snapshot.json")
}

fn copy_tree(from: &Path, to: &Path, reverse: bool) {
    fs::create_dir_all(to).unwrap();
    let mut entries: Vec<_> = fs::read_dir(from).unwrap().map(|e| e.unwrap()).collect();
    entries.sort_by_key(|e| e.file_name());
    if reverse {
        entries.reverse();
    }
    for e in entries {
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dest, reverse);
        } else {
            fs::copy(e.path(), dest).unwrap();
        }
    }
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-graph-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn matches_golden_snapshot_and_is_repeatable() {
    let graph = analyze(&fixture()).unwrap();
    let text = graph.to_snapshot_json();
    if std::env::var_os("CHIP_UPDATE_GOLDEN").is_some() {
        fs::write(golden(), &text).unwrap();
    }
    let expected = fs::read_to_string(golden()).unwrap();
    assert_eq!(
        text, expected,
        "golden snapshot differs; rerun with CHIP_UPDATE_GOLDEN=1 if intended"
    );
    for _ in 0..3 {
        assert_eq!(analyze(&fixture()).unwrap().snapshot_id, graph.snapshot_id);
    }
    assert!(graph.is_consistent());
    assert!(graph.snapshot_id.as_str().starts_with("sha256:"));
}

#[test]
fn fixture_has_expected_structure() {
    let g = analyze(&fixture()).unwrap();
    let has = |from: &str, kind, to: &str| {
        g.edges
            .iter()
            .any(|e| e.from == from && e.kind == kind && e.to == to)
    };
    assert_eq!(g.count_nodes(GraphNodeKind::Crate), 2);
    assert_eq!(g.count_nodes(GraphNodeKind::BinaryTarget), 1);
    assert!(g.nodes.iter().any(|n| n.id == "repository:."));
    assert!(
        g.nodes
            .iter()
            .any(|n| n.id == "symbol:src/api.rs:api::Request")
    );
    assert!(!g.nodes.iter().any(|n| n.id.contains("private_helper")));
    assert!(has(
        "symbol:src/api.rs:api::Request",
        GraphEdgeKind::Implements,
        "symbol:helper/src/lib.rs:Summarize"
    ));
    assert!(has(
        "file:src/api.rs",
        GraphEdgeKind::Imports,
        "module:.:crate::execution"
    ));
    assert!(has(
        "file:src/lib.rs",
        GraphEdgeKind::Imports,
        "module:.:crate::api"
    ));
    assert!(has(
        "binary:.:demo",
        GraphEdgeKind::Targets,
        "module:.:bin/demo"
    ));
    assert!(has(
        "test:tests/integration.rs:handles_a_request",
        GraphEdgeKind::Tests,
        "symbol:src/api.rs:api::handle"
    ));
    assert!(has(
        "repository:.",
        GraphEdgeKind::Contains,
        "config:.github/workflows/test.yml"
    ));
    // Structural only: no semantic edge kinds exist.
    assert_eq!(GraphEdgeKind::ALL.len(), 6);
}

#[test]
fn ordering_never_affects_the_snapshot_id() {
    let base = analyze(&fixture()).unwrap();

    let mut nodes = base.nodes.clone();
    let mut edges = base.edges.clone();
    nodes.reverse();
    edges.reverse();
    assert_eq!(
        ArchitectureGraph::from_parts(nodes.clone(), edges.clone()).snapshot_id,
        base.snapshot_id
    );
    nodes.rotate_left(7);
    edges.rotate_left(11);
    let mut dup = nodes.clone();
    dup.extend(nodes.iter().take(5).cloned());
    assert_eq!(
        ArchitectureGraph::from_parts(dup, edges).snapshot_id,
        base.snapshot_id
    );

    // Different creation order on disk, different directory name and location.
    let a = temp("forward");
    let b = temp("reverse").join("some-other-name");
    copy_tree(&fixture(), &a, false);
    copy_tree(&fixture(), &b, true);
    assert_eq!(analyze(&a).unwrap().snapshot_id, base.snapshot_id);
    assert_eq!(analyze(&b).unwrap().snapshot_id, base.snapshot_id);
    let _ = fs::remove_dir_all(&a);
    let _ = fs::remove_dir_all(b.parent().unwrap());
}

#[test]
fn identities_never_leak_the_machine() {
    let g = analyze(&fixture()).unwrap();
    let text = g.to_snapshot_json();
    let abs = fixture().to_string_lossy().into_owned();
    assert!(!text.contains(&abs));
    assert!(!text.contains("/home/") && !text.contains("/tmp/") && !text.contains("\\"));
}

#[test]
fn changing_source_changes_the_snapshot_and_restoring_restores_it() {
    let dir = temp("mutation");
    copy_tree(&fixture(), &dir, false);
    let original = analyze(&dir).unwrap();

    let path = dir.join("src/api.rs");
    let source = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{source}\npub fn added() {{}}\n")).unwrap();
    let changed = analyze(&dir).unwrap();
    assert_ne!(changed.snapshot_id, original.snapshot_id);
    assert!(
        changed
            .nodes
            .iter()
            .any(|n| n.id == "symbol:src/api.rs:api::added")
    );

    fs::write(&path, &source).unwrap();
    assert_eq!(analyze(&dir).unwrap().snapshot_id, original.snapshot_id);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn source_is_the_authority() {
    let dir = temp("authority");
    copy_tree(&fixture(), &dir, false);
    let before = analyze(&dir).unwrap();
    assert!(
        before
            .nodes
            .iter()
            .any(|n| n.id == "symbol:src/api.rs:api::handle")
    );

    // Removing a symbol from source removes it from the graph; the old graph cannot keep it.
    let path = dir.join("src/api.rs");
    let source = fs::read_to_string(&path).unwrap();
    let removed = source.replace("pub fn handle", "fn handle");
    fs::write(&path, removed).unwrap();
    let after = analyze(&dir).unwrap();
    assert!(
        !after
            .nodes
            .iter()
            .any(|n| n.id == "symbol:src/api.rs:api::handle")
    );
    assert_ne!(after.snapshot_id, before.snapshot_id);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn no_mutation_api_is_exposed() {
    let lib = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap();
    for banned in [
        "add_node",
        "remove_node",
        "add_edge",
        "remove_edge",
        "rewrite_source",
        "write_source",
    ] {
        assert!(!lib.contains(banned), "{banned} must not exist");
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in fs::read_dir(src).unwrap() {
        let text = fs::read_to_string(entry.unwrap().path()).unwrap();
        for banned in [
            "pub fn add_node",
            "pub fn remove_node",
            "pub fn add_edge",
            "pub fn rewrite_source",
        ] {
            assert!(!text.contains(banned), "{banned} must not exist");
        }
    }
}

#[test]
fn a_tampered_snapshot_is_refused() {
    let g = analyze(&fixture()).unwrap();
    let tampered = g.to_snapshot_json().replace(
        "symbol:src/api.rs:api::handle",
        "symbol:src/api.rs:api::other",
    );
    assert!(ArchitectureGraph::from_snapshot_json(&tampered).is_err());
    assert!(ArchitectureGraph::from_snapshot_json(&g.to_snapshot_json()).is_ok());
}

#[test]
fn architecture_isolation() {
    let manifest =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    for banned in [
        "chip-core",
        "chip-local-ml",
        "chip-laya-reasoner",
        "fx-core",
        "fx-provider-http",
        "chip-compute",
        "reqwest",
        "hyper",
        "tokio",
        "ureq",
        "openai",
        "anthropic",
    ] {
        assert!(
            !manifest.contains(banned),
            "chip-graph must not depend on {banned}"
        );
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        let text = fs::read_to_string(&path).unwrap();
        for banned in [
            "std::process",
            "Command::new",
            "std::net",
            "reqwest",
            "tokio",
            "std::env",
        ] {
            assert!(
                !text.contains(banned),
                "{} must not use {banned}",
                path.display()
            );
        }
    }
}

#[test]
fn snapshot_store_round_trips_and_reports_missing() {
    let dir = temp("store");
    fs::create_dir_all(&dir).unwrap();
    assert!(matches!(
        chip_graph::read_latest(&dir),
        Err(chip_graph::StoreError::NotFound)
    ));
    let g = analyze(&fixture()).unwrap();
    let path = chip_graph::write_snapshot(&dir, &g).unwrap();
    assert!(path.to_string_lossy().contains(".chip/graph/sha256-"));
    assert_eq!(chip_graph::read_latest(&dir).unwrap(), g);
    let _ = fs::remove_dir_all(&dir);
}
