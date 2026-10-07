//! The baseline decision function: fixed, deterministic, and deliberately not a learned model.
//!
//! This PR establishes the decision ABI and its runtime economics, not decision quality.

use crate::abi::{CONTINUE, ESCALATE, Evidence, Impact};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Continue,
    Escalate,
}

impl Decision {
    /// The ABI result code.
    pub const fn code(self) -> u8 {
        match self {
            Decision::Continue => CONTINUE,
            Decision::Escalate => ESCALATE,
        }
    }

    pub const fn from_code(code: u8) -> Option<Decision> {
        match code {
            CONTINUE => Some(Decision::Continue),
            ESCALATE => Some(Decision::Escalate),
            _ => None,
        }
    }
}

/// A decision plus informational confidence. Confidence never changes the decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecisionResult {
    pub decision: Decision,
    pub confidence: f32,
}

impl DecisionResult {
    /// A fixed rule is exact, so its confidence is 1.0. The ABI returns only the decision
    /// code; richer outputs belong to a later ABI version, once this one is proven.
    pub const fn certain(decision: Decision) -> Self {
        Self {
            decision,
            confidence: 1.0,
        }
    }
}

/// Continue only when evidence is valid and the capability is unchanged. Stale or unknown
/// evidence, or an impacted capability, escalates.
pub fn decide_state(evidence: Evidence, impact: Impact) -> DecisionResult {
    DecisionResult::certain(match (evidence, impact) {
        (Evidence::Valid, Impact::Unchanged) => Decision::Continue,
        _ => Decision::Escalate,
    })
}
