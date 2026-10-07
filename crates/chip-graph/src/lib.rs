//! A deterministic architecture graph of a Rust repository.
//!
//! **Architecture graphs are projections, never authority.** The graph may narrow what Chip
//! considers relevant, but it may never establish what is true. If it disagrees with source,
//! tests, Compute receipts or evidence, those win. Nothing here edits source, executes it,
//! runs Cargo or Git, reads the environment, or reaches the network.
//!
//! The same repository content always yields the same graph, the same canonical bytes and
//! the same snapshot id, regardless of machine, filesystem ordering, absolute path or time.

mod analyze;
mod model;
mod store;

pub use analyze::{AnalysisStats, AnalyzeError, analyze, analyze_with_stats, find_repository_root};
pub use model::{
    ArchitectureGraph, GraphEdge, GraphEdgeKind, GraphNode, GraphNodeKind, SCHEMA_VERSION,
    SnapshotId,
};
pub use store::{StoreError, read_latest, write_snapshot};
