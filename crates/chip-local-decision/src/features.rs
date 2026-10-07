//! Feature schema `chip.decision-features.v1`.
//!
//! Every feature is a pure function of the `CapabilityDecisionState` and of the model's own
//! vocabulary. Nothing reads external state, and nothing gives arbitrary strings meaning: a
//! capability id or a fact is either *in the model's vocabulary* (it was seen in training and
//! has a learned weight) or it is *unfamiliar*, which can only count against `Continue`.
//!
//! Dense features, in order (`BASE_FEATURES` = 10):
//!
//! | # | name                | value                                                                     |
//! |---|---------------------|---------------------------------------------------------------------------|
//! | 0 | `evidence_valid`    | 1 if evidence is valid                                                    |
//! | 1 | `evidence_stale`    | 1 if evidence is stale                                                    |
//! | 2 | `evidence_unknown`  | 1 if evidence was never established                                       |
//! | 3 | `impact_unchanged`  | 1 if the capability is unchanged                                          |
//! | 4 | `impact_impacted`   | 1 if the capability is impacted                                           |
//! | 5 | `has_inputs`        | 1 if any typed input is present                                           |
//! | 6 | `input_count`       | number of inputs, capped at 16, divided by 16                             |
//! | 7 | `unfamiliar_facts`  | number of inputs whose `name=value` is not in the vocabulary (cap 16)     |
//! | 8 | `unfamiliar_capability` | 1 if the capability id is not in the vocabulary                       |
//! | 9 | `dropped_facts`     | 1 if more than 16 familiar facts were present (extras are ignored)        |
//!
//! Features 7-9 have fixed pessimistic weights, set by the trainer, never learned: they exist so
//! that anything the model has not seen can only push toward `Escalate`.
//!
//! Sparse features follow: one indicator per vocabulary capability, then one per vocabulary
//! fact token (`name` plus a typed value: bool, integer or text).
//!
//! The graph state contributes **no** feature. It is an opaque digest with no order or
//! meaning, and any function of it would only give a model something to memorise.

use chip_core::{CapabilityDecisionState, EvidenceState, ImpactState, InputValue};

pub const FEATURE_SCHEMA: &str = "chip.decision-features.v1";
pub const BASE_FEATURES: usize = 10;
/// Most familiar facts that contribute individual weights.
pub const MAX_TOKENS: usize = 16;

pub const BASE_NAMES: [&str; BASE_FEATURES] = [
    "evidence_valid",
    "evidence_stale",
    "evidence_unknown",
    "impact_unchanged",
    "impact_impacted",
    "has_inputs",
    "input_count",
    "unfamiliar_facts",
    "unfamiliar_capability",
    "dropped_facts",
];

/// Indices of the features whose weights are fixed pessimistic priors.
pub const FROZEN: [usize; 3] = [7, 8, 9];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum TokenValue {
    Bool(bool),
    Int(i64),
    Text(String),
}

/// One fact the model has a weight for: a name and a typed value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Token {
    pub name: String,
    pub value: TokenValue,
}

impl Token {
    pub fn of(name: &str, value: &InputValue) -> Token {
        Token {
            name: name.to_string(),
            value: match value {
                InputValue::Bool(b) => TokenValue::Bool(*b),
                InputValue::Integer(i) => TokenValue::Int(*i),
                InputValue::Text(t) => TokenValue::Text(t.clone()),
            },
        }
    }

    fn matches(&self, name: &str, value: &InputValue) -> bool {
        self.name == name
            && match (&self.value, value) {
                (TokenValue::Bool(a), InputValue::Bool(b)) => a == b,
                (TokenValue::Int(a), InputValue::Integer(b)) => a == b,
                (TokenValue::Text(a), InputValue::Text(b)) => a == b,
                _ => false,
            }
    }
}

/// What the model knows by name. Built from the training data and stored in the artifact.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Vocabulary {
    pub capabilities: Vec<String>,
    pub tokens: Vec<Token>,
}

/// An extracted feature vector, without allocation: a fixed dense part plus indices into the
/// vocabulary.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Features {
    pub base: [f32; BASE_FEATURES],
    pub capability: Option<u16>,
    pub tokens: [u16; MAX_TOKENS],
    pub token_count: usize,
}

/// Extracts features. Deterministic and allocation-free.
pub fn extract(vocab: &Vocabulary, state: &CapabilityDecisionState) -> Features {
    let mut base = [0f32; BASE_FEATURES];
    base[match state.evidence_state {
        EvidenceState::KnownValid => 0,
        EvidenceState::KnownStale => 1,
        EvidenceState::Unknown => 2,
    }] = 1.0;
    base[match state.impact {
        ImpactState::Unchanged => 3,
        ImpactState::Impacted => 4,
    }] = 1.0;
    let count = state.inputs.len();
    base[5] = f32::from(count > 0);
    base[6] = count.min(16) as f32 / 16.0;

    let capability = vocab
        .capabilities
        .iter()
        .position(|c| c == state.capability_id.as_str())
        .map(|i| i as u16);
    base[8] = f32::from(capability.is_none());

    let mut tokens = [0u16; MAX_TOKENS];
    let mut token_count = 0;
    let mut unfamiliar = 0usize;
    for (name, value) in &state.inputs {
        match vocab.tokens.iter().position(|t| t.matches(name, value)) {
            Some(index) if token_count < MAX_TOKENS => {
                tokens[token_count] = index as u16;
                token_count += 1;
            }
            Some(_) => base[9] = 1.0,
            None => unfamiliar += 1,
        }
    }
    base[7] = unfamiliar.min(16) as f32;
    Features {
        base,
        capability,
        tokens,
        token_count,
    }
}
