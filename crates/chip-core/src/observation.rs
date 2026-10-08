//! The observation boundary: an execution result, represented as semantic data.
//! An observation reports what the result says and nothing more; it never
//! causes work and never draws conclusions the result does not contain.

use std::error::Error;
use std::fmt;

use crate::{ExecutionEvidence, ExecutionId, ExecutionResult, ExecutionStatus};

/// What kind of execution outcome was observed. Deliberately generic: there
/// are no domain-specific kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationKind {
    ExecutionCompleted,
    ExecutionFailed,
    ExecutionCancelled,
}

impl ObservationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExecutionCompleted => "execution.completed",
            Self::ExecutionFailed => "execution.failed",
            Self::ExecutionCancelled => "execution.cancelled",
        }
    }
}

/// The agent-visible representation of one execution result. Identity, status,
/// output and receipt are carried over unchanged. `output` is `None` only when
/// the result had no output at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub execution_id: ExecutionId,
    pub kind: ObservationKind,
    pub status: ExecutionStatus,
    pub output: Option<String>,
    pub receipt_id: Option<String>,
    /// Runtime identity reported for the execution, carried unchanged. It is
    /// not part of [`render`](Self::render), so the model never sees it.
    pub evidence: Option<ExecutionEvidence>,
}

impl Observation {
    /// Deterministic, provider-neutral text form handed to the model. Every
    /// field is carried; string values are quoted and escaped, and the output
    /// comes last, so output text cannot alter the kind, status or identifiers.
    pub fn render(&self) -> String {
        let status = match self.status {
            ExecutionStatus::Success => "success",
            ExecutionStatus::Failure => "failure",
            ExecutionStatus::Cancelled => "cancelled",
        };
        let quoted = |value: &Option<String>| match value {
            Some(text) => quote(text),
            None => "none".to_string(),
        };
        format!(
            "Observation:\nkind: {}\nexecution_id: {}\nstatus: {}\nreceipt_id: {}\noutput: {}",
            self.kind.as_str(),
            quote(&self.execution_id.0),
            status,
            quoted(&self.receipt_id),
            quoted(&self.output),
        )
    }
}

fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:04x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationError {
    /// A valid observation cannot be built from this result.
    InvalidResult(String),
    /// No observer is available.
    ObserverUnavailable(String),
}

impl fmt::Display for ObservationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidResult(message) => write!(f, "invalid execution result: {message}"),
            Self::ObserverUnavailable(message) => write!(f, "observer unavailable: {message}"),
        }
    }
}

impl Error for ObservationError {}

/// Turns execution reality into an observation. Pure and synchronous.
pub trait Observer: Send + Sync {
    fn observe(&self, result: &ExecutionResult) -> Result<Observation, ObservationError>;
}

/// Deterministic observer: a field-for-field translation of the result.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExecutionObserver;

impl Observer for ExecutionObserver {
    fn observe(&self, result: &ExecutionResult) -> Result<Observation, ObservationError> {
        if result.id.0.trim().is_empty() {
            return Err(ObservationError::InvalidResult(
                "result has an empty execution id".to_string(),
            ));
        }
        let kind = match result.status {
            ExecutionStatus::Success => ObservationKind::ExecutionCompleted,
            ExecutionStatus::Failure => ObservationKind::ExecutionFailed,
            ExecutionStatus::Cancelled => ObservationKind::ExecutionCancelled,
        };
        Ok(Observation {
            execution_id: result.id.clone(),
            kind,
            status: result.status,
            output: (!result.output.is_empty()).then(|| result.output.clone()),
            receipt_id: result.receipt_id.clone(),
            evidence: result.evidence.clone(),
        })
    }
}
