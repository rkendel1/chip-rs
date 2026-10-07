//! The local reasoning boundary: a small, bounded judgment made locally before
//! anything is escalated to the model. A reasoner receives only the structured
//! facts Chip chooses to give it and returns a structured verdict. It cannot
//! perform work, record evidence, or reach any other part of the system, and its
//! verdict is a decision input, never a command.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::{CapabilityId, InputValue};

/// What Chip knows about evidence for the operation. Chip determines this; the
/// reasoner never inspects anything to find out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceState {
    KnownValid,
    KnownStale,
    Unknown,
}

/// Everything a reasoner is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningInput {
    pub capability: CapabilityId,
    pub inputs: BTreeMap<String, InputValue>,
    pub evidence: EvidenceState,
}

/// A bounded verdict. `Escalate` means the local reasoner is not confident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalReasoningResult {
    Continue { rationale: String },
    Escalate { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningError {
    /// No local reasoner is available.
    Unavailable(String),
    /// The reasoner could not produce a valid verdict.
    Failed(String),
}

impl fmt::Display for ReasoningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) => write!(f, "local reasoner unavailable: {message}"),
            Self::Failed(message) => write!(f, "local reasoning failed: {message}"),
        }
    }
}

impl Error for ReasoningError {}

/// Makes one cheap local judgment. Synchronous and pure with respect to Chip:
/// the only things it can see are the arguments.
pub trait LocalReasoner: Send + Sync {
    fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError>;
}

/// Deterministic reasoner with an explicit policy per evidence state. The default
/// continues on valid evidence and escalates on stale or unknown evidence.
#[derive(Debug, Clone)]
pub struct TestLocalReasoner {
    valid: LocalReasoningResult,
    stale: LocalReasoningResult,
    unknown: LocalReasoningResult,
}

impl Default for TestLocalReasoner {
    fn default() -> Self {
        Self {
            valid: LocalReasoningResult::Continue {
                rationale: "evidence is valid".to_string(),
            },
            stale: LocalReasoningResult::Escalate {
                reason: "evidence is stale".to_string(),
            },
            unknown: LocalReasoningResult::Escalate {
                reason: "no evidence".to_string(),
            },
        }
    }
}

impl TestLocalReasoner {
    pub fn on(mut self, state: EvidenceState, result: LocalReasoningResult) -> Self {
        match state {
            EvidenceState::KnownValid => self.valid = result,
            EvidenceState::KnownStale => self.stale = result,
            EvidenceState::Unknown => self.unknown = result,
        }
        self
    }
}

impl LocalReasoner for TestLocalReasoner {
    fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
        Ok(match input.evidence {
            EvidenceState::KnownValid => self.valid.clone(),
            EvidenceState::KnownStale => self.stale.clone(),
            EvidenceState::Unknown => self.unknown.clone(),
        })
    }
}
