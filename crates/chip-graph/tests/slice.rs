use std::fs;
use std::path::{Path, PathBuf};

use chip_graph::{
    ArchitectureGraph, CapabilityBinding, CapabilityCatalog, GraphEdge, GraphEdgeKind,
    GraphNodeKind, SliceError, SliceSelection, analyze, capability_slice,
    capability_slice_for_impact,
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/slice_fixture")
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
    let dir = std::env::temp_dir().join(format!("chip-slice-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn catalog_text() -> String {
    fs::read_to_string(fixture().join("capabilities.json")).unwrap()
}

fn token_in(root: &Path) -> (String, chip_graph::GraphSlice) {
    let graph = analyze(root).unwrap();
    let catalog = CapabilityCatalog::from_json(&catalog_text(), &graph).unwrap();
    let slice = capability_slice(&graph, &catalog, "cap.slice").unwrap();
    (slice.state_token().to_string(), slice)
}

fn mutated(name: &str, edit: impl FnOnce(&Path)) -> PathBuf {
    let dir = temp(name);
    copy_tree(&fixture(), &dir, false);
    edit(&dir);
    dir
}

#[test]
fn the_slice_is_the_declared_nodes_and_the_edges_between_them() {
    let (token, slice) = token_in(&fixture());
    let ids: Vec<&str> = slice.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "crate:a",
            "file:a/src/one.rs",
            "module:a:crate::one",
            "module:a:crate::two"
        ]
    );
    assert!(token.starts_with("sha256:") && token.len() == 71);
    for e in &slice.edges {
        assert!(ids.contains(&e.from.as_str()) && ids.contains(&e.to.as_str()));
    }
    assert!(slice.edges.iter().any(|e| e.from == "module:a:crate::one"
        && e.kind == GraphEdgeKind::Contains
        && e.to == "file:a/src/one.rs"));
    // Nothing was traversed beyond the declaration.
    assert!(
        !ids.iter()
            .any(|i| i.starts_with("symbol:") || i.contains("three"))
    );
}

#[test]
fn same_graph_same_slice_same_token_wherever_it_lives() {
    let (token, slice) = token_in(&fixture());
    assert_eq!(token_in(&fixture()), (token.clone(), slice.clone()));

    let a = temp("loc-a");
    let b = temp("loc-b").join("a-different-name");
    copy_tree(&fixture(), &a, false);
    copy_tree(&fixture(), &b, true);
    assert_eq!(token_in(&a).0, token);
    assert_eq!(token_in(&b).0, token);
    assert_eq!(token_in(&b).1, slice);
    let _ = fs::remove_dir_all(&a);
    let _ = fs::remove_dir_all(b.parent().unwrap());
}

#[test]
fn matches_the_golden_slice() {
    let (_, slice) = token_in(&fixture());
    let text = slice.render(None);
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/slice.cap.slice.txt");
    if std::env::var_os("CHIP_UPDATE_GOLDEN").is_some() {
        fs::write(&golden, &text).unwrap();
    }
    assert_eq!(text, fs::read_to_string(&golden).unwrap());
}

#[test]
fn a_relevant_structural_change_changes_the_token() {
    let (before, _) = token_in(&fixture());
    // one.rs now imports module two: a new edge between two nodes in the slice.
    let dir = mutated("relevant", |d| {
        fs::write(
            d.join("a/src/one.rs"),
            "use crate::two::helper;\n\npub fn run() {\n    helper();\n}\n",
        )
        .unwrap();
    });
    let (after, _) = token_in(&dir);
    assert_ne!(before, after);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_irrelevant_change_does_not_move_the_token() {
    let (before, slice_before) = token_in(&fixture());
    let dir = mutated("irrelevant", |d| {
        fs::write(
            d.join("a/src/three.rs"),
            "pub fn other() {}\npub fn more() {}\npub struct New;\n",
        )
        .unwrap();
        fs::write(d.join("a/src/extra.rs"), "pub fn extra() {}\n").unwrap();
    });
    let (after, slice_after) = token_in(&dir);
    assert_eq!(before, after);
    // The repository as a whole did change; only the relevant state was hashed.
    assert_ne!(slice_before.snapshot_id, slice_after.snapshot_id);
    assert_eq!(slice_before.nodes, slice_after.nodes);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_edge_outside_the_slice_does_not_move_the_token() {
    let (before, _) = token_in(&fixture());
    // three.rs now imports module one: an edge whose source is outside the slice.
    let dir = mutated("outside-edge", |d| {
        fs::write(
            d.join("a/src/three.rs"),
            "use crate::one::run;\n\npub fn other() {\n    run();\n}\n",
        )
        .unwrap();
    });
    let graph_before = analyze(&fixture()).unwrap();
    let graph_after = analyze(&dir).unwrap();
    assert!(graph_after.edges.len() > graph_before.edges.len());
    assert_eq!(token_in(&dir).0, before);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn removing_or_adding_an_edge_inside_the_slice_changes_the_token() {
    let graph = analyze(&fixture()).unwrap();
    let catalog = CapabilityCatalog::from_json(&catalog_text(), &graph).unwrap();
    let base = capability_slice(&graph, &catalog, "cap.slice").unwrap();

    let inside = |e: &GraphEdge| {
        base.nodes.iter().any(|n| n.id == e.from) && base.nodes.iter().any(|n| n.id == e.to)
    };
    let victim = graph.edges.iter().find(|e| inside(e)).unwrap().clone();
    let without: Vec<GraphEdge> = graph
        .edges
        .iter()
        .filter(|e| **e != victim)
        .cloned()
        .collect();
    let g2 = ArchitectureGraph::from_parts(graph.nodes.clone(), without);
    let s2 = capability_slice(&g2, &catalog, "cap.slice").unwrap();
    assert_ne!(base.state_token(), s2.state_token());

    let mut with = graph.edges.clone();
    with.push(GraphEdge {
        from: "module:a:crate::two".into(),
        kind: GraphEdgeKind::Imports,
        to: "module:a:crate::one".into(),
    });
    let g3 = ArchitectureGraph::from_parts(graph.nodes.clone(), with);
    assert_ne!(
        base.state_token(),
        capability_slice(&g3, &catalog, "cap.slice")
            .unwrap()
            .state_token()
    );

    // An edge with an endpoint outside the slice leaves it alone.
    let mut outside = graph.edges.clone();
    outside.push(GraphEdge {
        from: "module:a:crate::three".into(),
        kind: GraphEdgeKind::Imports,
        to: "module:a:crate::one".into(),
    });
    let g4 = ArchitectureGraph::from_parts(graph.nodes.clone(), outside);
    assert_eq!(
        base.state_token(),
        capability_slice(&g4, &catalog, "cap.slice")
            .unwrap()
            .state_token()
    );
}

#[test]
fn the_token_commits_to_kind_capability_and_schema_but_not_to_order() {
    let graph = analyze(&fixture()).unwrap();
    let catalog = CapabilityCatalog::from_json(&catalog_text(), &graph).unwrap();
    let base = capability_slice(&graph, &catalog, "cap.slice").unwrap();

    let mut kind = base.clone();
    kind.nodes[0].kind = GraphNodeKind::Module;
    assert_ne!(kind.state_token(), base.state_token());

    let mut renamed = base.clone();
    renamed.capability_id = "cap.renamed".into();
    assert_ne!(renamed.state_token(), base.state_token());

    let mut shuffled = base.clone();
    shuffled.nodes.reverse();
    shuffled.edges.reverse();
    shuffled.edges.extend(base.edges.iter().cloned());
    assert_eq!(shuffled.state_token(), base.state_token());

    // Provenance is not hashed.
    let mut other_snapshot = base.clone();
    other_snapshot.snapshot_id = analyze(&fixture().join("a/..")).unwrap().snapshot_id;
    assert_eq!(other_snapshot.state_token(), base.state_token());

    let bytes = String::from_utf8(base.canonical_bytes()).unwrap();
    assert!(bytes.contains("chip.slice.v1") && bytes.contains("chip.architecture.v1"));
    assert!(!bytes.contains("/home/") && !bytes.contains("/tmp/") && !bytes.contains('\n'));
}

#[test]
fn graph_and_catalog_ordering_never_move_the_token() {
    let graph = analyze(&fixture()).unwrap();
    let catalog = CapabilityCatalog::from_json(&catalog_text(), &graph).unwrap();
    let base = capability_slice(&graph, &catalog, "cap.slice")
        .unwrap()
        .state_token();

    let mut nodes = graph.nodes.clone();
    let mut edges = graph.edges.clone();
    nodes.reverse();
    edges.reverse();
    nodes.rotate_left(3);
    let shuffled = ArchitectureGraph::from_parts(nodes, edges);
    assert_eq!(shuffled.snapshot_id, graph.snapshot_id);
    assert_eq!(
        capability_slice(&shuffled, &catalog, "cap.slice")
            .unwrap()
            .state_token(),
        base
    );

    // Reordered declarations, reordered and repeated node lists, without normalization.
    let hand = CapabilityCatalog {
        schema: catalog.schema.clone(),
        capabilities: vec![
            CapabilityBinding {
                id: "cap.other".into(),
                graph_nodes: vec!["module:a:crate::three".into()],
            },
            CapabilityBinding {
                id: "cap.slice".into(),
                graph_nodes: vec![
                    "module:a:crate::two".into(),
                    "crate:a".into(),
                    "module:a:crate::one".into(),
                    "crate:a".into(),
                    "file:a/src/one.rs".into(),
                ],
            },
        ],
    };
    assert_eq!(
        capability_slice(&graph, &hand, "cap.slice")
            .unwrap()
            .state_token(),
        base
    );
}

#[test]
fn errors_are_explicit_and_never_yield_a_token() {
    let graph = analyze(&fixture()).unwrap();
    let catalog = CapabilityCatalog::from_json(&catalog_text(), &graph).unwrap();
    assert_eq!(
        capability_slice(&graph, &catalog, "cap.nope").unwrap_err(),
        SliceError::UnknownCapability("cap.nope".into())
    );

    let empty = CapabilityCatalog {
        schema: catalog.schema.clone(),
        capabilities: vec![CapabilityBinding {
            id: "cap.empty".into(),
            graph_nodes: vec![],
        }],
    };
    assert_eq!(
        capability_slice(&graph, &empty, "cap.empty").unwrap_err(),
        SliceError::EmptySlice("cap.empty".into())
    );

    let stale = CapabilityCatalog {
        schema: catalog.schema.clone(),
        capabilities: vec![CapabilityBinding {
            id: "cap.stale".into(),
            graph_nodes: vec!["crate:a".into(), "file:a/src/gone.rs".into()],
        }],
    };
    assert!(matches!(
        capability_slice(&graph, &stale, "cap.stale").unwrap_err(),
        SliceError::UnknownNode { .. }
    ));

    // PR20 validation still guards the catalog: deleting a declared file makes it invalid.
    let dir = mutated("deleted", |d| {
        fs::remove_file(d.join("a/src/two.rs")).unwrap();
        fs::write(d.join("a/src/lib.rs"), "pub mod one;\npub mod three;\n").unwrap();
    });
    let after = analyze(&dir).unwrap();
    assert!(CapabilityCatalog::from_json(&catalog_text(), &after).is_err());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn selection_reports_whether_the_capability_was_reached() {
    let graph = analyze(&fixture()).unwrap();
    let catalog = CapabilityCatalog::from_json(&catalog_text(), &graph).unwrap();
    let paths = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    let hit = capability_slice_for_impact(&graph, &catalog, "cap.slice", &paths(&["a/src/one.rs"]))
        .unwrap();
    assert!(matches!(hit, SliceSelection::Impacted(_)));

    // a/src/lib.rs belongs to crate:a, which cap.slice declares.
    let krate =
        capability_slice_for_impact(&graph, &catalog, "cap.slice", &paths(&["a/src/lib.rs"]))
            .unwrap();
    assert!(krate.is_impacted());

    let miss =
        capability_slice_for_impact(&graph, &catalog, "cap.other", &paths(&["a/src/one.rs"]))
            .unwrap();
    assert!(matches!(miss, SliceSelection::Unchanged(_)));
    // Unchanged still reports the capability's real slice, token and all.
    assert_eq!(
        miss.slice(),
        &capability_slice(&graph, &catalog, "cap.other").unwrap()
    );

    let unresolved =
        capability_slice_for_impact(&graph, &catalog, "cap.slice", &paths(&["nope.rs"])).unwrap();
    assert!(!unresolved.is_impacted());
    let none = capability_slice_for_impact(&graph, &catalog, "cap.slice", &[]).unwrap();
    assert!(!none.is_impacted());
    assert!(capability_slice_for_impact(&graph, &catalog, "cap.nope", &[]).is_err());
}

#[test]
fn a_tampered_snapshot_is_still_refused() {
    let graph = analyze(&fixture()).unwrap();
    let tampered = graph.to_snapshot_json().replace("crate:a", "crate:zz");
    assert!(ArchitectureGraph::from_snapshot_json(&tampered).is_err());
}
