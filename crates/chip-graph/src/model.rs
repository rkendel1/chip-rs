//! The graph model and its canonical, content-addressed serialization.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SCHEMA_VERSION: &str = "chip.architecture.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphNodeKind {
    Repository,
    Crate,
    Module,
    File,
    Symbol,
    TestSuite,
    BinaryTarget,
    ConfigSurface,
}

impl GraphNodeKind {
    pub const ALL: [GraphNodeKind; 8] = [
        GraphNodeKind::Repository,
        GraphNodeKind::Crate,
        GraphNodeKind::Module,
        GraphNodeKind::File,
        GraphNodeKind::Symbol,
        GraphNodeKind::TestSuite,
        GraphNodeKind::BinaryTarget,
        GraphNodeKind::ConfigSurface,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphEdgeKind {
    Contains,
    Imports,
    Defines,
    Implements,
    Tests,
    Targets,
}

impl GraphEdgeKind {
    pub const ALL: [GraphEdgeKind; 6] = [
        GraphEdgeKind::Contains,
        GraphEdgeKind::Imports,
        GraphEdgeKind::Defines,
        GraphEdgeKind::Implements,
        GraphEdgeKind::Tests,
        GraphEdgeKind::Targets,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            GraphEdgeKind::Contains => "contains",
            GraphEdgeKind::Imports => "imports",
            GraphEdgeKind::Defines => "defines",
            GraphEdgeKind::Implements => "implements",
            GraphEdgeKind::Tests => "tests",
            GraphEdgeKind::Targets => "targets",
        }
    }
}

/// A node. The id is repository-relative and deterministic; it never contains an absolute
/// path, user name, machine name, timestamp or environment value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub kind: GraphNodeKind,
}

/// A structural relationship found in source. Never a semantic claim.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: String,
    pub kind: GraphEdgeKind,
    pub to: String,
}

impl GraphEdge {
    fn sort_key(&self) -> (&str, &'static str, &str) {
        (&self.from, self.kind.as_str(), &self.to)
    }
}

/// `sha256:<hex>` of the canonical graph bytes. A function of graph content only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotId(String);

impl SnapshotId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A file-name-safe form (`sha256-<hex>`).
    pub fn file_stem(&self) -> String {
        self.0.replace(':', "-")
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchitectureGraph {
    pub schema_version: String,
    pub snapshot_id: SnapshotId,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// What is hashed: the graph without its own id, in fixed field order.
#[derive(Serialize)]
struct Canonical<'a> {
    schema_version: &'a str,
    nodes: &'a [GraphNode],
    edges: &'a [GraphEdge],
}

fn canonical_bytes(nodes: &[GraphNode], edges: &[GraphEdge]) -> Vec<u8> {
    serde_json::to_vec(&Canonical {
        schema_version: SCHEMA_VERSION,
        nodes,
        edges,
    })
    .expect("a graph of strings and enums always serializes")
}

fn digest(nodes: &[GraphNode], edges: &[GraphEdge]) -> SnapshotId {
    let hash = Sha256::digest(canonical_bytes(nodes, edges));
    let mut hex = String::with_capacity(71);
    hex.push_str("sha256:");
    for byte in hash {
        hex.push_str(&format!("{byte:02x}"));
    }
    SnapshotId(hex)
}

impl ArchitectureGraph {
    /// Builds a graph from unordered parts. Nodes and edges are sorted and de-duplicated, so
    /// the order they arrive in never affects the result or the snapshot id.
    pub fn from_parts(nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) -> ArchitectureGraph {
        let nodes: Vec<GraphNode> = nodes
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut edges = edges;
        edges.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        edges.dedup();
        let snapshot_id = digest(&nodes, &edges);
        ArchitectureGraph {
            schema_version: SCHEMA_VERSION.to_string(),
            snapshot_id,
            nodes,
            edges,
        }
    }

    /// The exact bytes the snapshot id is the SHA-256 of.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        canonical_bytes(&self.nodes, &self.edges)
    }

    /// Whether the recorded id matches the content.
    pub fn is_consistent(&self) -> bool {
        self.schema_version == SCHEMA_VERSION
            && digest(&self.nodes, &self.edges) == self.snapshot_id
    }

    pub fn count_nodes(&self, kind: GraphNodeKind) -> usize {
        self.nodes.iter().filter(|n| n.kind == kind).count()
    }

    pub fn count_edges(&self, kind: GraphEdgeKind) -> usize {
        self.edges.iter().filter(|e| e.kind == kind).count()
    }

    /// The human-readable snapshot: schema, id, nodes, edges, in a fixed order.
    pub fn to_snapshot_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).expect("a graph always serializes");
        text.push('\n');
        text
    }

    /// Reads a snapshot and refuses one whose id does not match its content.
    pub fn from_snapshot_json(text: &str) -> Result<ArchitectureGraph, String> {
        let graph: ArchitectureGraph =
            serde_json::from_str(text).map_err(|e| format!("not a valid snapshot: {e}"))?;
        if !graph.is_consistent() {
            return Err("snapshot id does not match its content".to_string());
        }
        Ok(graph)
    }
}
