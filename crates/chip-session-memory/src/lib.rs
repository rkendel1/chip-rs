//! EXPERIMENT: native FeltDB as isolated, disposable session working memory for Chip.
//!
//! See `docs/product/session-memory-experiment.md` for the question, the method, the findings and
//! the go/no-go recommendation. This crate is a leaf: no other crate depends on it, it is disabled
//! unless a test or benchmark constructs a [`SessionMemory`], and it can be removed by deleting
//! this directory and its line in the workspace `members`.
//!
//! Session memory is *working state*. It is not evidence that anything executed or succeeded: an
//! execution identity stored here is an opaque string the caller supplied, and nothing here mints
//! or derives one.

pub mod backend;
pub mod compaction;
pub mod error;
pub mod packet;
pub mod recovery;
pub mod schema;
pub mod store;
pub mod workload;

pub use backend::{Backend, Felt, Journal, Mutation, Redb, Sqlite};
pub use compaction::{CompactionReport, ReclaimReport};
pub use error::{CompactionPhase, Result, SessionMemoryError};
pub use recovery::RecoveredSession;
pub use schema::*;
pub use store::{Capabilities, NewObservation, SessionMemory, Stats, Support, capabilities};
