mod common;

use chip_core::{EvidenceState, ImpactState, InputValue};
use chip_local_decision::{
    BASE_FEATURES, Decision, Features, LocalDecisionModel, MAX_TOKENS, ModelError, extract,
};
use common::*;

fn embedded() -> LocalDecisionModel {
    LocalDecisionModel::embedded().expect("the committed artifact parses")
}

#[test]
fn feature_extraction_is_documented_and_stable() {
    let vocab = small_vocabulary();
    let s = state(
        "tests.run",
        EvidenceState::KnownStale,
        ImpactState::Unchanged,
    )
    .with_input("flag", InputValue::Bool(true))
    .with_input("n", InputValue::Integer(2))
    .with_input("surprise", InputValue::Text("?".into()))
    .with_input("note", InputValue::Text("other".into()));
    let f = extract(&vocab, &s);
    // evidence_valid, stale, unknown | impact_unchanged, impacted | has_inputs | input_count/16 |
    // unfamiliar_facts | unfamiliar_capability | dropped_facts
    assert_eq!(
        f.base,
        [0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 4.0 / 16.0, 2.0, 0.0, 0.0]
    );
    assert_eq!(f.capability, Some(1));
    assert_eq!(&f.tokens[..f.token_count], &[0, 1]);
    assert_eq!(extract(&vocab, &s), f, "repeatable");

    // Same value, different type: not the same fact.
    let typed = state("tests.run", EvidenceState::Unknown, ImpactState::Impacted)
        .with_input("flag", InputValue::Text("true".into()));
    let g = extract(&vocab, &typed);
    assert_eq!(g.token_count, 0);
    assert_eq!(g.base[7], 1.0);
    // Unfamiliar capability.
    assert_eq!(
        extract(
            &vocab,
            &state("never.seen", EvidenceState::Unknown, ImpactState::Impacted)
        )
        .base[8],
        1.0
    );
    // The graph digest contributes nothing.
    let mut other = s.clone();
    other.graph_state = chip_core::GraphStateToken::from_digest([1; 32]);
    assert_eq!(extract(&vocab, &other), f);
}

#[test]
fn facts_beyond_the_token_budget_are_flagged_not_silently_dropped() {
    let mut vocab = small_vocabulary();
    vocab.tokens.clear();
    let mut s = state("tests.run", EvidenceState::Unknown, ImpactState::Unchanged);
    for i in 0..(MAX_TOKENS + 2) {
        vocab.tokens.push(chip_local_decision::Token {
            name: format!("f{i:02}"),
            value: chip_local_decision::TokenValue::Bool(true),
        });
        s = s.with_input(format!("f{i:02}"), InputValue::Bool(true));
    }
    let f = extract(&vocab, &s);
    assert_eq!(f.token_count, MAX_TOKENS);
    assert_eq!(f.base[9], 1.0, "dropped_facts is set");
}

#[test]
fn inference_is_deterministic_and_repeatable() {
    let model = embedded();
    for s in all_states() {
        let first = model.infer(&s);
        for _ in 0..100 {
            assert_eq!(model.infer(&s), first);
        }
        assert_eq!(embedded().infer(&s), first, "a freshly loaded model agrees");
    }
}

#[test]
fn the_artifact_serialization_is_stable_and_round_trips() {
    let model = embedded();
    let bytes = model.to_bytes();
    assert_eq!(
        bytes,
        include_bytes!("../models/local-decision-v1.bin"),
        "re-serializing reproduces the committed bytes"
    );
    assert_eq!(LocalDecisionModel::from_bytes(&bytes).unwrap(), model);
    assert_eq!(
        bytes,
        LocalDecisionModel::from_bytes(&bytes).unwrap().to_bytes()
    );
    assert!(bytes.len() < 4096, "artifact is {} bytes", bytes.len());
}

#[test]
fn an_invalid_artifact_fails_closed() {
    let good = embedded().to_bytes();

    assert_eq!(
        LocalDecisionModel::from_bytes(&[]),
        Err(ModelError::Truncated)
    );
    assert_eq!(
        LocalDecisionModel::from_bytes(b"not a model at all"),
        Err(ModelError::BadMagic)
    );

    for cut in 0..good.len() {
        assert!(
            LocalDecisionModel::from_bytes(&good[..cut]).is_err(),
            "truncated at {cut}"
        );
    }
    let mut extra = good.clone();
    extra.push(0);
    assert!(
        LocalDecisionModel::from_bytes(&extra).is_err(),
        "trailing byte"
    );

    // Any single damaged byte is caught, in the header, the vocabulary or the weights.
    for at in 0..good.len() {
        let mut bad = good.clone();
        bad[at] ^= 0x01;
        assert!(
            LocalDecisionModel::from_bytes(&bad).is_err(),
            "flipped byte {at}"
        );
    }
}

fn with_checksum(mut body: Vec<u8>) -> Vec<u8> {
    // Recompute the CRC-32 trailer, so only the field under test is wrong.
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in &body {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    body.extend_from_slice(&(!crc).to_le_bytes());
    body
}

#[test]
fn an_unsupported_version_or_feature_schema_fails_closed() {
    let good = embedded().to_bytes();
    let body = &good[..good.len() - 4];

    let mut version = body.to_vec();
    version[8..10].copy_from_slice(&2u16.to_le_bytes());
    assert_eq!(
        LocalDecisionModel::from_bytes(&with_checksum(version)),
        Err(ModelError::UnsupportedVersion(2))
    );

    let mut schema = body.to_vec();
    schema[10..12].copy_from_slice(&9u16.to_le_bytes());
    assert_eq!(
        LocalDecisionModel::from_bytes(&with_checksum(schema)),
        Err(ModelError::UnsupportedFeatureSchema(9))
    );

    let mut base = body.to_vec();
    base[12..14].copy_from_slice(&((BASE_FEATURES as u16) + 1).to_le_bytes());
    assert!(matches!(
        LocalDecisionModel::from_bytes(&with_checksum(base)),
        Err(ModelError::UnsupportedFeatureSchema(_))
    ));

    // A non-finite weight is refused even behind a valid checksum.
    let mut nan = body.to_vec();
    let at = nan.len() - 4;
    nan[at..].copy_from_slice(&f32::NAN.to_le_bytes());
    assert_eq!(
        LocalDecisionModel::from_bytes(&with_checksum(nan)),
        Err(ModelError::NonFinite)
    );
}

#[test]
fn invalid_feature_values_fail_closed() {
    let model = embedded();
    let good = extract(
        model.vocabulary(),
        &state(
            "tests.run",
            EvidenceState::KnownStale,
            ImpactState::Unchanged,
        ),
    );
    assert!(model.infer_features(&good).is_ok());

    let mut nan = good;
    nan.base[0] = f32::NAN;
    assert_eq!(model.infer_features(&nan), Err(ModelError::InvalidFeatures));
    let mut inf = good;
    inf.base[6] = f32::INFINITY;
    assert_eq!(model.infer_features(&inf), Err(ModelError::InvalidFeatures));
    let mut capability = good;
    capability.capability = Some(60_000);
    assert_eq!(
        model.infer_features(&capability),
        Err(ModelError::InvalidFeatures)
    );
    let mut token = good;
    token.tokens[0] = 60_000;
    token.token_count = 1;
    assert_eq!(
        model.infer_features(&token),
        Err(ModelError::InvalidFeatures)
    );
    let mut count = good;
    count.token_count = MAX_TOKENS + 1;
    assert_eq!(
        model.infer_features(&count),
        Err(ModelError::InvalidFeatures)
    );

    let _ = Features { ..good };
}

#[test]
fn a_tie_or_a_non_finite_margin_is_an_escalation() {
    // threshold == margin exactly: not strictly greater, so Escalate.
    let zeros = vec![0.0f32; BASE_FEATURES];
    let model = LocalDecisionModel::new(
        Default::default(),
        [zeros.clone(), zeros.clone()],
        [1.0, -1.0],
        2.0,
    )
    .unwrap();
    let s = state("x.y", EvidenceState::Unknown, ImpactState::Unchanged);
    assert_eq!(model.infer(&s).decision, Decision::Escalate);
    let just_below = LocalDecisionModel::new(
        Default::default(),
        [zeros.clone(), zeros.clone()],
        [1.0, -1.0],
        1.9,
    )
    .unwrap();
    assert_eq!(just_below.infer(&s).decision, Decision::Continue);
    assert!(
        LocalDecisionModel::new(
            Default::default(),
            [zeros.clone(), zeros],
            [f32::NAN, 0.0],
            0.0
        )
        .is_err()
    );
}

#[test]
fn unfamiliar_facts_can_only_lower_the_chance_of_continuing() {
    let model = embedded();
    let (w_continue, w_escalate) = model.weights();
    for index in [7usize, 8, 9] {
        assert!(
            w_continue[index] - w_escalate[index] < 0.0,
            "feature {index} must carry a pessimistic weight"
        );
    }
    // Adding an unseen fact never raises the margin.
    let base = state(
        "tests.run",
        EvidenceState::KnownStale,
        ImpactState::Unchanged,
    )
    .with_input("prerequisites_met", InputValue::Bool(true))
    .with_input("change_affects_capability", InputValue::Bool(false));
    let with_unseen = base
        .clone()
        .with_input("a_fact_nobody_trained_on", InputValue::Bool(true));
    assert!(model.infer(&with_unseen).margin < model.infer(&base).margin);
}
