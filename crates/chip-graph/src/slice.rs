//! A capability's relevant graph slice and the content-addressed state token derived from it.
//!
//! The slice is a projection, never a new source of truth: the architecture graph stays
//! authoritative for source-derived structure, and the catalog is only external metadata
//! saying which nodes matter. The first relevance boundary is deliberately conservative and
//! auditable: exactly the nodes the capability declared, plus the graph edges whose two
//! endpoints are both among them. Nothing is traversed, ranked, guessed or asked of a model.
//!
//! The token hashes the slice's structure (node ids and kinds, edge kinds and endpoints),
//! not the repository, so identical architecture gives an identical token wherever the
//! repository lives. It hashes *structure only*: the graph does not see function bodies, so an
//! edit that leaves the declared nodes and the edges between them unchanged leaves the token
//! unchanged.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::impact::{CapabilityCatalog, analyze_impact};
use crate::model::{ArchitectureGraph, GraphEdge, GraphNode, SCHEMA_VERSION, SnapshotId};

/// Version of the canonical slice encoding that is hashed.
pub const SLICE_SCHEMA: &str = "chip.slice.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SliceError {
    UnknownCapability(String),
    /// A declared node is not in the graph (a stale or mismatched catalog).
    UnknownNode {
        capability: String,
        node: String,
    },
    /// The capability declares no nodes; there is nothing to hash, so there is no token.
    EmptySlice(String),
}

impl fmt::Display for SliceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SliceError::UnknownCapability(id) => write!(f, "unknown capability: {id}"),
            SliceError::UnknownNode { capability, node } => {
                write!(
                    f,
                    "capability {capability} names a node not in the architecture snapshot: {node}"
                )
            }
            SliceError::EmptySlice(id) => write!(f, "capability {id} has an empty relevant slice"),
        }
    }
}

impl std::error::Error for SliceError {}

/// `sha256:<hex>` of the canonical slice. A function of the slice's structure only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct GraphStateToken(String);

impl GraphStateToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GraphStateToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSlice {
    pub capability_id: String,
    /// Sorted by id. Carries the node kind because the token commits to it.
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// The snapshot this was cut from. Provenance only: it is not part of the token, so a
    /// change elsewhere in the repository does not move the token.
    pub snapshot_id: SnapshotId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SliceSelection {
    /// The changed paths do not reach this capability.
    Unchanged(GraphSlice),
    /// At least one declared node of this capability is in the changed set.
    Impacted(GraphSlice),
}

impl SliceSelection {
    pub fn slice(&self) -> &GraphSlice {
        match self {
            SliceSelection::Unchanged(s) | SliceSelection::Impacted(s) => s,
        }
    }

    pub fn is_impacted(&self) -> bool {
        matches!(self, SliceSelection::Impacted(_))
    }
}

#[derive(Serialize)]
struct CanonicalNode<'a> {
    id: &'a str,
    kind: crate::model::GraphNodeKind,
}

#[derive(Serialize)]
struct CanonicalEdge<'a> {
    from: &'a str,
    kind: crate::model::GraphEdgeKind,
    to: &'a str,
}

#[derive(Serialize)]
struct CanonicalSlice<'a> {
    slice_schema: &'a str,
    graph_schema: &'a str,
    capability_id: &'a str,
    nodes: Vec<CanonicalNode<'a>>,
    edges: Vec<CanonicalEdge<'a>>,
}

impl GraphSlice {
    /// The exact bytes the token is the SHA-256 of. Order-independent: nodes and edges are
    /// sorted here, whatever order they were built in.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let nodes: BTreeSet<(&str, crate::model::GraphNodeKind)> =
            self.nodes.iter().map(|n| (n.id.as_str(), n.kind)).collect();
        let edges: BTreeSet<(&str, crate::model::GraphEdgeKind, &str)> = self
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.kind, e.to.as_str()))
            .collect();
        let canonical = CanonicalSlice {
            slice_schema: SLICE_SCHEMA,
            graph_schema: SCHEMA_VERSION,
            capability_id: &self.capability_id,
            nodes: nodes
                .into_iter()
                .map(|(id, kind)| CanonicalNode { id, kind })
                .collect(),
            edges: edges
                .into_iter()
                .map(|(from, kind, to)| CanonicalEdge { from, kind, to })
                .collect(),
        };
        serde_json::to_vec(&canonical).expect("a slice of strings and enums always serializes")
    }

    pub fn state_token(&self) -> GraphStateToken {
        let hash = Sha256::digest(self.canonical_bytes());
        let mut hex = String::with_capacity(71);
        hex.push_str("sha256:");
        for byte in hash {
            hex.push_str(&format!("{byte:02x}"));
        }
        GraphStateToken(hex)
    }

    /// The human-readable report. `impact` is added only when changed paths were supplied.
    pub fn render(&self, impact: Option<&str>) -> String {
        let mut out = format!(
            "Capability:\n  {}\n\nSnapshot:\n  {}\n\n",
            self.capability_id, self.snapshot_id
        );
        if let Some(impact) = impact {
            out.push_str(&format!("Impact:\n  {impact}\n\n"));
        }
        out.push_str("Nodes:\n");
        for node in &self.nodes {
            out.push_str(&format!("  {}\n", node.id));
        }
        out.push_str("\nEdges:\n");
        if self.edges.is_empty() {
            out.push_str("  none\n");
        }
        for edge in &self.edges {
            out.push_str(&format!(
                "  {} {} {}\n",
                edge.from,
                edge.kind.as_str(),
                edge.to
            ));
        }
        out.push_str(&format!("\nStateToken:\n  {}\n", self.state_token()));
        out
    }
}

/// The capability's declared nodes and the edges between them, nothing more.
pub fn capability_slice(
    graph: &ArchitectureGraph,
    catalog: &CapabilityCatalog,
    capability_id: &str,
) -> Result<GraphSlice, SliceError> {
    let capability = catalog
        .capabilities
        .iter()
        .find(|c| c.id == capability_id)
        .ok_or_else(|| SliceError::UnknownCapability(capability_id.to_string()))?;

    let known: BTreeMap<&str, &GraphNode> =
        graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let declared: BTreeSet<&str> = capability.graph_nodes.iter().map(String::as_str).collect();
    if declared.is_empty() {
        return Err(SliceError::EmptySlice(capability_id.to_string()));
    }
    let mut nodes = Vec::with_capacity(declared.len());
    for id in &declared {
        match known.get(id) {
            Some(node) => nodes.push((*node).clone()),
            None => {
                return Err(SliceError::UnknownNode {
                    capability: capability_id.to_string(),
                    node: (*id).to_string(),
                });
            }
        }
    }
    let edges = graph
        .edges
        .iter()
        .filter(|e| declared.contains(e.from.as_str()) && declared.contains(e.to.as_str()))
        .cloned()
        .collect();
    Ok(GraphSlice {
        capability_id: capability_id.to_string(),
        nodes,
        edges,
        snapshot_id: graph.snapshot_id.clone(),
    })
}

/// The same slice, tagged by whether the changed paths reach the capability. A capability
/// that is not impacted is reported as such; it is never given a fresh slice as if it were.
pub fn capability_slice_for_impact(
    graph: &ArchitectureGraph,
    catalog: &CapabilityCatalog,
    capability_id: &str,
    changed_paths: &[String],
) -> Result<SliceSelection, SliceError> {
    let slice = capability_slice(graph, catalog, capability_id)?;
    let report = analyze_impact(graph, catalog, changed_paths);
    if report
        .impacted_capabilities
        .iter()
        .any(|c| c.id == capability_id)
    {
        Ok(SliceSelection::Impacted(slice))
    } else {
        Ok(SliceSelection::Unchanged(slice))
    }
}
