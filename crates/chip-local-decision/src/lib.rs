//! A tiny learned local decision model.
//!
//! It consumes the same `CapabilityDecisionState` as the deterministic baseline and answers the
//! same question: `Continue` or `Escalate`. There is no text, tokenizer, JSON, prompt, network,
//! filesystem access or generation anywhere in the hot path. The model is a linear classifier
//! with two logits over a small, explicit, versioned feature vector.
//!
//! The deterministic policy is permanently available and is the safety fallback. A learned
//! `Continue` is only ever advice: [`PolicyMode::LearnedGuarded`] refuses it whenever a
//! non-relaxable safety condition fails, and with no model loaded every mode is the
//! deterministic policy.
//!
//! Training lives elsewhere (`chip-local-decision-train`). The runtime holds only the compact
//! parameters.

pub mod eval;
pub mod features;
pub mod model;
pub mod policy;

pub use chip_wasm_decision::Decision;
pub use features::{
    BASE_FEATURES, FEATURE_SCHEMA, Features, MAX_TOKENS, Token, TokenValue, Vocabulary, extract,
};
pub use model::{ARTIFACT_VERSION, Inference, LocalDecisionModel, MODEL_SCHEMA, ModelError};
pub use policy::{LocalDecider, PolicyMode, PolicyOutcome, deterministic_decision};
