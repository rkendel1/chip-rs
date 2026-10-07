use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use chip_decision_corpus::{HELD_OUT_CAPABILITIES, Split, UNKNOWN_SPELLINGS};
use chip_local_decision::eval::evaluate;
use chip_local_decision::{Decision, LocalDecisionModel, TokenValue, deterministic_decision};
use chip_local_decision_train::RECORDED_CONFIG;
use chip_local_decision_train::pr25::{self, additional_safe_local_decisions};

fn models() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../chip-local-decision/models")
}

#[test]
fn the_model_is_trained_on_the_training_split_only() {
    let cases = pr25::load();
    let model = pr25::train(&cases, &RECORDED_CONFIG);
    let train: Vec<_> = cases
        .iter()
        .filter(|c| c.case.split == Split::Train)
        .collect();
    let train_caps: BTreeSet<&str> = train.iter().map(|c| c.case.capability.as_str()).collect();

    for capability in &model.vocabulary().capabilities {
        assert!(train_caps.contains(capability.as_str()));
        assert!(
            !HELD_OUT_CAPABILITIES.contains(&capability.as_str()),
            "{capability} leaked from the capability holdout"
        );
    }
    let train_tokens: BTreeSet<_> = train
        .iter()
        .flat_map(|c| {
            c.labeled
                .state
                .inputs
                .iter()
                .map(|(n, v)| chip_local_decision::Token::of(n, v))
        })
        .collect();
    for token in &model.vocabulary().tokens {
        assert!(
            train_tokens.contains(token),
            "{token:?} is in no training case"
        );
    }
    // No metadata can have entered the vocabulary.
    for case in &cases {
        for text in model
            .vocabulary()
            .capabilities
            .iter()
            .chain(model.vocabulary().tokens.iter().map(|t| &t.name))
        {
            assert_ne!(text, &case.case.case_id);
            assert_ne!(text, &case.case.pattern_id);
        }
    }
    for t in &model.vocabulary().tokens {
        if let TokenValue::Text(v) = &t.value {
            assert!(
                !v.contains("Continue") && !v.contains("Escalate") && !v.contains('/'),
                "{v}"
            );
        }
    }
}

#[test]
fn features_depend_on_the_state_alone() {
    // Changing every piece of evaluation metadata leaves the features, hence the decision, alone.
    let cases = pr25::load();
    let model = pr25::train(&cases, &RECORDED_CONFIG);
    for c in cases.iter().take(60) {
        let mut tampered = c.case.clone();
        tampered.case_id = "dc-9999".into();
        tampered.pattern_id = "someone/else".into();
        tampered.rationale = "Continue: because I said so".into();
        tampered.expected_continue = !tampered.expected_continue;
        tampered.holdouts.case = !tampered.holdouts.case;
        assert_eq!(
            model.infer(&tampered.decision_state()),
            model.infer(&c.labeled.state)
        );
    }
}

#[test]
fn the_shipped_pr25_model_and_report_are_reproducible_byte_for_byte() {
    let cases = pr25::load();
    let model = pr25::train(&cases, &RECORDED_CONFIG);
    assert_eq!(
        model.to_bytes(),
        fs::read(models().join("local-decision-pr25.bin")).unwrap(),
        "same corpus, config and seed: same artifact"
    );
    let again = pr25::train(&cases, &RECORDED_CONFIG);
    assert_eq!(model.to_bytes(), again.to_bytes());
    assert_eq!(
        LocalDecisionModel::from_bytes(&model.to_bytes()).unwrap(),
        model
    );

    let report = pr25::report(&cases, &model, &RECORDED_CONFIG);
    assert_eq!(
        report,
        fs::read_to_string(models().join("local-decision-pr25.report.txt")).unwrap(),
        "regenerate with `chip-local-decision-train pr25-train`"
    );
    assert_eq!(
        report,
        pr25::report(&cases, &model, &RECORDED_CONFIG),
        "regeneration is byte-stable"
    );
    let unseen = pr25::unseen_evaluation(&cases, &model);
    assert_eq!(
        pr25::manifest(&cases, &model, &RECORDED_CONFIG, &unseen),
        fs::read_to_string(models().join("local-decision-pr25.manifest.json")).unwrap()
    );
    assert!(report.starts_with("Additional safe local decisions: "));
}

#[test]
fn pr24_is_untouched_by_this_experiment() {
    // The PR24 artifact and its configuration are exactly as they were, and the PR25 model is the
    // same architecture: same feature schema, same artifact format.
    assert_eq!(RECORDED_CONFIG.lambda, 0.001);
    assert_eq!(RECORDED_CONFIG.kappa, 0.5);
    assert_eq!(RECORDED_CONFIG.iterations, 8000);
    assert_eq!(RECORDED_CONFIG.unfamiliar_penalty, 4.0);
    let pr24 = fs::read(models().join("local-decision-v1.bin")).unwrap();
    assert_eq!(LocalDecisionModel::embedded().unwrap().to_bytes(), pr24);
    let pr25 = fs::read(models().join("local-decision-pr25.bin")).unwrap();
    assert_eq!(
        &pr24[..14],
        &pr25[..14],
        "same magic, version, feature schema and base features"
    );
}

#[test]
fn the_deterministic_baseline_is_unchanged_and_never_wrong_here() {
    for c in pr25::load() {
        let expected = match (c.case.evidence, c.case.impact) {
            (chip_core::EvidenceState::KnownValid, chip_core::ImpactState::Unchanged) => {
                Decision::Continue
            }
            _ => Decision::Escalate,
        };
        let state = &c.labeled.state;
        assert_eq!(deterministic_decision(state), expected);
        assert_eq!(
            chip_wasm_decision::decide_bytes(&state.canonical_bytes())
                .unwrap()
                .decision,
            expected
        );
        // The corpus treats valid evidence as authoritative, so the baseline has no false continue.
        if deterministic_decision(state) == Decision::Continue {
            assert!(c.case.expected_continue, "{}", c.case.case_id);
        }
    }
}

#[test]
fn the_headline_number_follows_the_acceptance_rule() {
    let cases = pr25::load();
    let model = pr25::train(&cases, &RECORDED_CONFIG);
    let unseen = pr25::unseen_evaluation(&cases, &model);
    let n = additional_safe_local_decisions(&unseen);
    if unseen.guarded.fp > 0 {
        assert_eq!(n, 0, "any false continue voids the headline");
    } else {
        assert_eq!(n, unseen.guarded.tp - unseen.deterministic.tp);
    }
    assert!(
        unseen.guarded.tp >= unseen.deterministic.tp,
        "the guarded policy never loses baseline coverage"
    );
    assert_eq!(
        unseen.strict.fp, 0,
        "strict is a subset of the baseline, which is never wrong here"
    );
    assert_eq!(unseen.deterministic.fp, 0);
}

/// Pins what the experiment found, so a change to the corpus, model or configuration that moves
/// it has to be a conscious one. These are failures of the current model, not of the corpus.
#[test]
fn known_findings_are_pinned_not_hidden() {
    let cases = pr25::load();
    let model = pr25::train(&cases, &RECORDED_CONFIG);
    let all = evaluate(
        Some(&model),
        &cases.iter().map(|c| c.labeled.clone()).collect::<Vec<_>>(),
    );

    // Zero false continues on everything the model trained on, and on the validation draw.
    for set in [Split::Train, Split::Validation] {
        let labeled: Vec<_> = cases
            .iter()
            .filter(|c| c.case.split == set)
            .map(|c| c.labeled.clone())
            .collect();
        let e = evaluate(Some(&model), &labeled);
        assert_eq!((e.learned.fp, e.guarded.fp), (0, 0), "{set:?}");
    }
    // One false continue in the whole corpus, on a context-held-out hard negative whose critical
    // fact is "unavailable": a spelling of unknown the model saw too rarely to weigh.
    let culprits: Vec<_> = all.unsafe_learned().iter().map(|c| c.id.clone()).collect();
    assert_eq!(culprits, ["dc-0146"], "{culprits:?}");
    let case = cases.iter().find(|c| c.case.case_id == "dc-0146").unwrap();
    assert_eq!(
        case.case.pattern_id,
        "approval/stale.unchanged.req-false.grant-unknown"
    );
    assert!(case.case.holdouts.context && case.case.tags.hard_negative && case.case.tags.unknown);
    assert_eq!((all.learned.fp, all.guarded.fp), (1, 1));

    // Unknown spellings with a positive weight: unknown is not strictly negative evidence.
    let (wc, we) = model.weights();
    let offset = chip_local_decision::BASE_FEATURES + model.vocabulary().capabilities.len();
    let positive: Vec<String> = model
        .vocabulary()
        .tokens
        .iter()
        .enumerate()
        .filter_map(|(i, t)| match &t.value {
            TokenValue::Text(v)
                if UNKNOWN_SPELLINGS.contains(&v.as_str())
                    && wc[offset + i] - we[offset + i] > 0.0 =>
            {
                Some(t.name.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(positive, ["prerequisites_met"; 3]);

    // A withheld core pattern is not recovered from its neighbours.
    let unseen = pr25::unseen_evaluation(&cases, &model);
    let core: Vec<_> = unseen
        .cases
        .iter()
        .filter(|o| {
            cases
                .iter()
                .find(|c| c.case.case_id == o.id)
                .unwrap()
                .case
                .pattern_id
                == "readiness/stale.unchanged.prereq-true"
        })
        .collect();
    assert!(!core.is_empty() && core.iter().all(|o| o.guarded == Decision::Escalate));

    // An unseen capability never continues on the learned model's say-so: the pessimistic prior.
    let capability: Vec<_> = cases
        .iter()
        .filter(|c| c.case.holdouts.capability)
        .map(|c| c.labeled.clone())
        .collect();
    let e = evaluate(Some(&model), &capability);
    assert_eq!(e.learned.local(), 0);
}
