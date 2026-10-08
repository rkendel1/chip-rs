//! The smallest schema the experiment needs.
//!
//! Ten logical categories, stored as eleven FeltDB collections (the extra one is the compaction
//! record). Every record carries its schema version and its session; every key is
//! `{collection}:{session}:{id}`. Large transient payloads live only in the `payload` collection,
//! so they can be deleted without touching the small record that describes them.
//!
//! Nothing here is evidence. A record says what a session *held*; the identity of an execution is
//! whatever opaque string the caller supplied, kept verbatim and never interpreted, minted or
//! derived from a session id.

use serde::{Deserialize, Serialize};

use crate::error::{Result, SessionMemoryError};

pub const SCHEMA_VERSION: u32 = 1;

pub mod coll {
    pub const SESSION: &str = "session";
    pub const TASK: &str = "task";
    pub const PLAN: &str = "plan";
    pub const ATTEMPT: &str = "attempt";
    pub const OBS: &str = "obs";
    pub const PAYLOAD: &str = "payload";
    pub const TEST: &str = "test";
    pub const REPAIR: &str = "repair";
    pub const HYP: &str = "hyp";
    pub const ESC: &str = "esc";
    pub const CKPT: &str = "ckpt";
    pub const COMPACTION: &str = "compaction";
    pub const ALL: [&str; 12] = [
        SESSION, TASK, PLAN, ATTEMPT, OBS, PAYLOAD, TEST, REPAIR, HYP, ESC, CKPT, COMPACTION,
    ];
}

/// A validated identifier: 1 to 64 of `[A-Za-z0-9_.-]`. No `:`, which FeltDB keys use as the
/// collection separator.
pub fn valid_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(SessionMemoryError::Invalid(format!(
            "identifier {id:?} must be 1 to 64 of [A-Za-z0-9_.-]"
        )))
    }
}

pub fn key(collection: &str, session: &str, id: &str) -> String {
    format!("{collection}:{session}:{id}")
}

/// How long a piece of information must be kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    /// Needed to continue the current task: the plan, open tasks, open hypotheses.
    Working,
    /// Needed to reconstruct the task, understand earlier attempts, resume or escalate:
    /// attempts, repairs, test results, escalations, checkpoints, observation summaries.
    Recovery,
    /// Redundant or superseded once its useful information is preserved: raw payloads.
    Transient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Escalated,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingAction {
    pub description: String,
    pub decision_needed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub schema: u32,
    pub session: String,
    pub objective: String,
    pub status: SessionStatus,
    pub pending: Option<PendingAction>,
    pub last_checkpoint: Option<String>,
    /// Completed compactions. Written only after a compaction finished and verified.
    pub compactions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub schema: u32,
    pub session: String,
    pub id: String,
    pub goal: String,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRecord {
    pub schema: u32,
    pub session: String,
    pub revision: u32,
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub schema: u32,
    pub session: String,
    pub id: String,
    pub action: String,
    pub outcome: Outcome,
    pub refs: Vec<Reference>,
}

/// An observation as the session holds it. `payload_len` and `digest` describe the transient
/// payload whether or not it is still stored; `excerpt` is the part kept for recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationRecord {
    pub schema: u32,
    pub session: String,
    pub id: String,
    pub kind: String,
    /// Opaque provenance supplied by the caller, kept verbatim: for example the identity of an
    /// execution some other authority issued. Never minted, derived or interpreted here.
    pub provenance: Option<String>,
    pub summary: String,
    pub digest: String,
    pub payload_len: u64,
    pub excerpt: String,
    /// The payload is still stored in the `payload` collection.
    pub payload_held: bool,
    /// An observation that supersedes this one (a duplicate or a newer reading).
    pub superseded_by: Option<String>,
    /// Required by recovery: an unresolved failure's diagnostic must keep its payload.
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadRecord {
    pub schema: u32,
    pub session: String,
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestResultRecord {
    pub schema: u32,
    pub session: String,
    pub id: String,
    /// Identity of the command or test, as the caller names it.
    pub command: String,
    pub passed: bool,
    pub failed_tests: Vec<String>,
    /// The observation holding the diagnostic.
    pub diagnostic: Option<String>,
    /// A later result for the same command replaced this one as the current reading.
    pub superseded_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairRecord {
    pub schema: u32,
    pub session: String,
    pub id: String,
    pub fix: String,
    pub outcome: Outcome,
    /// The passing test result that verifies this repair. Set only after the adapter has checked
    /// that the result exists and passed; a claim in text is not verification.
    pub verified_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HypothesisRecord {
    pub schema: u32,
    pub session: String,
    pub id: String,
    pub explanation: String,
    pub evidence: Vec<Reference>,
    /// Parts per thousand, when the caller has one.
    pub confidence_permille: Option<u16>,
    pub open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscalationRecord {
    pub schema: u32,
    pub session: String,
    pub id: String,
    pub reason: String,
    pub prior_attempts: Vec<String>,
    pub known_failures: Vec<String>,
    pub successes: Vec<String>,
    pub outstanding: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub schema: u32,
    pub session: String,
    pub id: String,
    /// FeltDB's sequence number when this was written.
    pub sequence: u64,
    /// FeltDB's digest of its own state at that point.
    pub state_digest: String,
    /// Everything a recovery needs to find, by reference.
    pub refs: Vec<Reference>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionState {
    /// The compacted representation is persisted and verified; the purge has not finished.
    Preserved,
    /// The purge finished and was verified.
    Purged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub schema: u32,
    pub session: String,
    pub number: u32,
    pub state: CompactionState,
    /// Observation ids whose payloads are to be purged, fixed when the record is written.
    pub purge: Vec<String>,
    /// Payloads no observation record claims: the leftover of a write interrupted between the
    /// payload and its record. They hold no recoverable information, so they are purged too.
    #[serde(default)]
    pub orphans: Vec<String>,
    pub purge_bytes: u64,
    pub preserved_records: u64,
    pub preserved_bytes: u64,
}
