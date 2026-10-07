//! Generic capability impact analysis.
//!
//! Capability identity may cross the Chip boundary; capability implementation does not.
//!
//! Chip consumes a capability ID plus graph-node bindings and nothing else. It does not care
//! whether the catalog came from another application, a CI system, a human or a generated
//! file. The catalog is external metadata, never authority: the architecture graph stays
//! derived from source, and a catalog that names a node the graph does not contain is
//! rejected rather than trusted.
//!
//! The algorithm is deliberately plain: changed paths resolve to File nodes, each File brings
//! the Modules and Crates that contain it, and a capability is impacted when at least one of
//! its declared nodes is in that set. No matching by meaning, no models, no Git, no clock.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::{ArchitectureGraph, GraphEdgeKind, GraphNodeKind};

pub const CATALOG_SCHEMA: &str = "chip.capabilities.v1";

/// External capability metadata: which graph nodes are relevant to which capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityCatalog {
    pub schema: String,
    pub capabilities: Vec<CapabilityBinding>,
}

/// The only meaning of `graph_nodes`: these graph node IDs are relevant to this capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityBinding {
    pub id: String,
    pub graph_nodes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    NotJson(String),
    WrongSchema(String),
    Malformed(String),
    EmptyId,
    DuplicateId(String),
    EmptyGraphNodes(String),
    UnknownGraphNode { capability: String, node: String },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogError::NotJson(m) => write!(f, "capability catalog is not valid JSON: {m}"),
            CatalogError::WrongSchema(s) => {
                write!(
                    f,
                    "capability catalog schema must be {CATALOG_SCHEMA}, found {s}"
                )
            }
            CatalogError::Malformed(m) => write!(f, "capability catalog is malformed: {m}"),
            CatalogError::EmptyId => f.write_str("capability id must not be empty"),
            CatalogError::DuplicateId(id) => write!(f, "duplicate capability id: {id}"),
            CatalogError::EmptyGraphNodes(id) => {
                write!(f, "capability {id} must declare at least one graph node")
            }
            CatalogError::UnknownGraphNode { capability, node } => {
                write!(
                    f,
                    "capability {capability} names a node not in the architecture snapshot: {node}"
                )
            }
        }
    }
}

impl std::error::Error for CatalogError {}

impl CapabilityCatalog {
    /// Parses and validates a catalog against the graph it will be applied to. The result is
    /// normalized: capabilities sorted by id, each capability's nodes sorted and de-duplicated.
    pub fn from_json(
        text: &str,
        graph: &ArchitectureGraph,
    ) -> Result<CapabilityCatalog, CatalogError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| CatalogError::NotJson(e.to_string()))?;
        match value.get("schema").and_then(|s| s.as_str()) {
            Some(CATALOG_SCHEMA) => {}
            Some(other) => return Err(CatalogError::WrongSchema(other.to_string())),
            None => return Err(CatalogError::WrongSchema("(missing)".to_string())),
        }
        let catalog: CapabilityCatalog =
            serde_json::from_value(value).map_err(|e| CatalogError::Malformed(e.to_string()))?;
        catalog.validated(graph)
    }

    fn validated(mut self, graph: &ArchitectureGraph) -> Result<CapabilityCatalog, CatalogError> {
        let known: BTreeSet<&str> = graph.nodes.iter().map(|n| n.id.as_str()).collect();
        let mut seen = BTreeSet::new();
        for capability in &mut self.capabilities {
            if capability.id.trim().is_empty() {
                return Err(CatalogError::EmptyId);
            }
            if !seen.insert(capability.id.clone()) {
                return Err(CatalogError::DuplicateId(capability.id.clone()));
            }
            if capability.graph_nodes.is_empty() {
                return Err(CatalogError::EmptyGraphNodes(capability.id.clone()));
            }
            capability.graph_nodes.sort();
            capability.graph_nodes.dedup();
            if let Some(node) = capability
                .graph_nodes
                .iter()
                .find(|n| !known.contains(n.as_str()))
            {
                return Err(CatalogError::UnknownGraphNode {
                    capability: capability.id.clone(),
                    node: node.clone(),
                });
            }
        }
        self.capabilities.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImpactedCapability {
    pub id: String,
    pub matched_nodes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImpactReport {
    pub changed_nodes: Vec<String>,
    pub impacted_capabilities: Vec<ImpactedCapability>,
    /// Paths Chip could not map to the graph. "Could not map" is not "no impact".
    pub unresolved_paths: Vec<String>,
}

fn normalize_path(path: &str) -> String {
    let path = path
        .strip_prefix("file:")
        .unwrap_or(path)
        .replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// Which capabilities are potentially impacted by changes to `paths`.
///
/// `catalog` should come from [`CapabilityCatalog::from_json`] for this same graph.
pub fn analyze_impact(
    graph: &ArchitectureGraph,
    catalog: &CapabilityCatalog,
    paths: &[String],
) -> ImpactReport {
    let kinds: BTreeMap<&str, GraphNodeKind> = graph
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.kind))
        .collect();
    // For each node, the Modules and Crates that contain it.
    let mut containers: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for edge in graph
        .edges
        .iter()
        .filter(|e| e.kind == GraphEdgeKind::Contains)
    {
        if matches!(
            kinds.get(edge.from.as_str()),
            Some(GraphNodeKind::Module | GraphNodeKind::Crate)
        ) {
            containers
                .entry(edge.to.as_str())
                .or_default()
                .push(edge.from.as_str());
        }
    }

    let mut changed: BTreeSet<String> = BTreeSet::new();
    let mut unresolved: BTreeSet<String> = BTreeSet::new();
    for path in paths {
        let normalized = normalize_path(path);
        let node = [format!("file:{normalized}"), format!("config:{normalized}")]
            .into_iter()
            .find(|id| kinds.contains_key(id.as_str()));
        match node {
            Some(id) => {
                for container in containers.get(id.as_str()).into_iter().flatten() {
                    changed.insert((*container).to_string());
                }
                changed.insert(id);
            }
            None => {
                unresolved.insert(normalized);
            }
        }
    }

    let impacted_capabilities = catalog
        .capabilities
        .iter()
        .filter_map(|capability| {
            let matched: BTreeSet<&String> = capability
                .graph_nodes
                .iter()
                .filter(|n| changed.contains(*n))
                .collect();
            (!matched.is_empty()).then(|| ImpactedCapability {
                id: capability.id.clone(),
                matched_nodes: matched.into_iter().cloned().collect(),
            })
        })
        .collect::<Vec<_>>();
    let mut impacted_capabilities = impacted_capabilities;
    impacted_capabilities.sort_by(|a, b| a.id.cmp(&b.id));

    ImpactReport {
        changed_nodes: changed.into_iter().collect(),
        impacted_capabilities,
        unresolved_paths: unresolved.into_iter().collect(),
    }
}

impl ImpactReport {
    /// The human-readable report. A pure function of the report: same report, same bytes.
    pub fn render(&self) -> String {
        let mut out = String::from("Changed:\n");
        if self.changed_nodes.is_empty() {
            out.push_str("  none\n");
        }
        for node in &self.changed_nodes {
            out.push_str(&format!("  {node}\n"));
        }
        out.push_str("\nImpacted capabilities:\n");
        if self.impacted_capabilities.is_empty() {
            out.push_str("  none\n");
        }
        for (i, capability) in self.impacted_capabilities.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&format!("  {}\n    matched:\n", capability.id));
            for node in &capability.matched_nodes {
                out.push_str(&format!("      {node}\n"));
            }
        }
        out.push_str("\nUnresolved:\n");
        if self.unresolved_paths.is_empty() {
            out.push_str("  none\n");
        }
        for path in &self.unresolved_paths {
            out.push_str(&format!("  {path}\n"));
        }
        out
    }
}
