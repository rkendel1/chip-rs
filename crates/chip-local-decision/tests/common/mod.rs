#![allow(dead_code)]

use chip_core::{
    CapabilityDecisionState, CapabilityId, EvidenceState, GraphStateToken, ImpactState, InputValue,
};
use chip_local_decision::{BASE_FEATURES, LocalDecisionModel, Token, TokenValue, Vocabulary};

pub fn state(
    capability: &str,
    evidence: EvidenceState,
    impact: ImpactState,
) -> CapabilityDecisionState {
    CapabilityDecisionState::new(
        CapabilityId::new(capability).unwrap(),
        GraphStateToken::from_digest([0x5a; 32]),
        evidence,
        impact,
    )
}

pub fn all_states() -> Vec<CapabilityDecisionState> {
    let mut out = Vec::new();
    for evidence in [
        EvidenceState::KnownValid,
        EvidenceState::KnownStale,
        EvidenceState::Unknown,
    ] {
        for impact in [ImpactState::Unchanged, ImpactState::Impacted] {
            out.push(state("tests.run", evidence, impact));
            out.push(
                state("tests.run", evidence, impact)
                    .with_input("prerequisites_met", InputValue::Bool(true))
                    .with_input("change_affects_capability", InputValue::Bool(false)),
            );
        }
    }
    out
}

pub fn small_vocabulary() -> Vocabulary {
    Vocabulary {
        capabilities: vec!["a.b".into(), "tests.run".into()],
        tokens: vec![
            Token {
                name: "flag".into(),
                value: TokenValue::Bool(true),
            },
            Token {
                name: "n".into(),
                value: TokenValue::Int(2),
            },
            Token {
                name: "note".into(),
                value: TokenValue::Text("x".into()),
            },
        ],
    }
}

/// A model that says Continue to everything: stands in for a badly wrong learned model, so the
/// guards are exercised against a worst case.
pub fn always_continue() -> LocalDecisionModel {
    let vocab = Vocabulary::default();
    let zeros = vec![0.0f32; BASE_FEATURES];
    LocalDecisionModel::new(vocab, [zeros.clone(), zeros], [10.0, -10.0], 0.0).unwrap()
}

pub fn never_continue() -> LocalDecisionModel {
    let zeros = vec![0.0f32; BASE_FEATURES];
    LocalDecisionModel::new(
        Vocabulary::default(),
        [zeros.clone(), zeros],
        [-10.0, 10.0],
        0.0,
    )
    .unwrap()
}
