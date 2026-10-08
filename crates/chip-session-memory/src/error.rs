//! Typed errors. Every failure names the stage that failed, so a caller (and a test) can tell an
//! initialization failure from a persistence, query, compaction, checkpoint or recovery failure.

use std::fmt;

/// Where in a compaction a failure or an injected interruption happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionPhase {
    Select,
    Preserve,
    Validate,
    Purge,
    Verify,
    Record,
    Reclaim,
}

#[derive(Debug)]
pub enum SessionMemoryError {
    /// Creating the store failed. Anything partially created has been removed.
    Init(String),
    /// A write, update or delete did not reach the store.
    Persist(String),
    /// A read or a query failed, or a stored record could not be decoded.
    Query(String),
    /// Compaction failed or was interrupted. `preserved` says whether the preservation step had
    /// completed: the session is never marked compacted in either case.
    Compaction {
        phase: CompactionPhase,
        message: String,
    },
    /// A checkpoint could not be written or is inconsistent.
    Checkpoint(String),
    /// A session could not be reconstructed from persisted data.
    Recovery(String),
    /// A record was addressed to another session.
    ForeignRecord { expected: String, found: String },
    /// A record names something that does not exist.
    MissingReference { kind: &'static str, id: String },
    /// An identifier or record violates the schema.
    Invalid(String),
    /// The session was closed.
    Closed,
    /// A capability FeltDB's native API does not offer. Never emulated.
    Unsupported(&'static str),
}

impl fmt::Display for SessionMemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use SessionMemoryError::*;
        match self {
            Init(m) => write!(f, "initialization failed: {m}"),
            Persist(m) => write!(f, "persistence failed: {m}"),
            Query(m) => write!(f, "query failed: {m}"),
            Compaction { phase, message } => write!(f, "compaction failed in {phase:?}: {message}"),
            Checkpoint(m) => write!(f, "checkpoint failed: {m}"),
            Recovery(m) => write!(f, "recovery failed: {m}"),
            ForeignRecord { expected, found } => {
                write!(f, "record belongs to session {found}, not {expected}")
            }
            MissingReference { kind, id } => write!(f, "no {kind} named {id}"),
            Invalid(m) => write!(f, "invalid: {m}"),
            Closed => write!(f, "the session memory is closed"),
            Unsupported(what) => write!(f, "unsupported by the native FeltDB API: {what}"),
        }
    }
}

impl std::error::Error for SessionMemoryError {}

pub type Result<T> = std::result::Result<T, SessionMemoryError>;
