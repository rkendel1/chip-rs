//! Policy modes: how a learned answer is allowed to become a decision.
//!
//! The deterministic policy (valid evidence and an unchanged capability continue; everything
//! else escalates) is always computed and always available. A learned `Continue` is advice.
//!
//! *Non-relaxable* conditions are the ones a learned model may never override. Here that is the
//! capability's impact: an impacted capability is never continued on the model's say-so. The
//! *relaxable* condition is the evidence one: the learned model exists precisely to continue some
//! cases the baseline escalates because evidence is stale or unknown.
//!
//! [`PolicyMode::LearnedGuarded`] therefore continues where the baseline continues (the fallback
//! is never weakened), plus where the model continues and the capability is unchanged. The
//! literal reading "learned AND deterministic" makes the guarded decision a subset of the
//! baseline's, so it can never resolve anything new (and can lose baseline coverage); it is kept
//! as [`PolicyMode::LearnedStrict`] so the difference is measurable.

use chip_core::{CapabilityDecisionState, EvidenceState, ImpactState};
use chip_wasm_decision::Decision;

use crate::model::LocalDecisionModel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyMode {
    /// The baseline only.
    Deterministic,
    /// The classifier's answer, unguarded. For measurement.
    Learned,
    /// The baseline's `Continue`, plus the classifier's while the capability is unchanged.
    LearnedGuarded,
    /// The classifier may continue only where the baseline also continues.
    LearnedStrict,
}

/// The baseline: valid evidence and an unchanged capability continue; everything else
/// escalates. The same rule as the Wasm decision module.
pub fn deterministic_decision(state: &CapabilityDecisionState) -> Decision {
    match (state.evidence_state, state.impact) {
        (EvidenceState::KnownValid, ImpactState::Unchanged) => Decision::Continue,
        _ => Decision::Escalate,
    }
}

/// Everything one decision produced, so unsafe learned answers are never hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyOutcome {
    pub deterministic: Decision,
    /// The classifier's raw answer; `None` when no model is loaded.
    pub learned_raw: Option<Decision>,
    pub guarded: Decision,
    pub strict: Decision,
    /// The decision under the decider's mode.
    pub decision: Decision,
}

pub struct LocalDecider {
    model: Option<LocalDecisionModel>,
    mode: PolicyMode,
}

impl LocalDecider {
    pub fn new(model: Option<LocalDecisionModel>, mode: PolicyMode) -> Self {
        Self { model, mode }
    }

    pub fn mode(&self) -> PolicyMode {
        self.mode
    }

    pub fn model(&self) -> Option<&LocalDecisionModel> {
        self.model.as_ref()
    }

    /// Removes the learned model. Every mode then behaves as the deterministic policy.
    pub fn disable_model(&mut self) -> Option<LocalDecisionModel> {
        self.model.take()
    }

    pub fn decide(&self, state: &CapabilityDecisionState) -> PolicyOutcome {
        let deterministic = deterministic_decision(state);
        let learned_raw = self.model.as_ref().map(|m| m.infer(state).decision);
        let learned = learned_raw.unwrap_or(Decision::Escalate);
        // The baseline's own `Continue` is always honoured; a learned `Continue` adds to it only
        // while the non-relaxable condition (impact unchanged) holds.
        let guarded = match (deterministic, learned, state.impact) {
            (Decision::Continue, _, _) => Decision::Continue,
            (_, Decision::Continue, ImpactState::Unchanged) => Decision::Continue,
            _ => Decision::Escalate,
        };
        let strict = match (learned, deterministic) {
            (Decision::Continue, Decision::Continue) => Decision::Continue,
            _ => Decision::Escalate,
        };
        let decision = match (&self.model, self.mode) {
            (None, _) | (_, PolicyMode::Deterministic) => deterministic,
            (Some(_), PolicyMode::Learned) => learned,
            (Some(_), PolicyMode::LearnedGuarded) => guarded,
            (Some(_), PolicyMode::LearnedStrict) => strict,
        };
        PolicyOutcome {
            deterministic,
            learned_raw,
            guarded,
            strict,
            decision,
        }
    }
}
