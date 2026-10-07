use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use chip_local_decision::LocalDecisionModel;
use chip_local_decision::eval::evaluate;
use chip_local_decision::features::Token;
use chip_local_decision_train::report::{full_report, manifest_json};
use chip_local_decision_train::{
    RECORDED_CONFIG, artifact_sha256, corpus_digest, labeled_corpus, leave_one_out, mlp, select,
    split, train_final,
};

fn models_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../chip-local-decision/models")
}

#[test]
fn the_split_is_seeded_disjoint_and_covers_the_corpus() {
    let cases = labeled_corpus();
    assert_eq!(cases.len(), 32);
    let a = split(&cases, 42);
    assert_eq!(a, split(&cases, 42), "same seed, same split");
    assert_ne!(a, split(&cases, 7), "the seed matters");

    let mut all: Vec<usize> = a
        .train
        .iter()
        .chain(&a.validation)
        .chain(&a.held_out)
        .copied()
        .collect();
    all.sort_unstable();
    assert_eq!(all, (0..32).collect::<Vec<_>>(), "disjoint and complete");
    for part in [&a.train, &a.validation, &a.held_out] {
        assert!(
            part.iter()
                .any(|&i| cases[i].expected == chip_local_decision::Decision::Continue)
        );
        assert!(
            part.iter()
                .any(|&i| cases[i].expected == chip_local_decision::Decision::Escalate)
        );
    }
    assert_eq!(
        (a.train.len(), a.validation.len(), a.held_out.len()),
        (18, 7, 7)
    );
}

#[test]
fn the_shipped_model_is_reproducible_from_corpus_features_config_and_seed() {
    let cases = labeled_corpus();
    let (model, sp) = train_final(&cases, &RECORDED_CONFIG);
    let committed = fs::read(models_dir().join("local-decision-v1.bin")).unwrap();
    assert_eq!(
        model.to_bytes(),
        committed,
        "retraining reproduces the committed artifact byte for byte"
    );
    assert_eq!(
        LocalDecisionModel::embedded().unwrap().to_bytes(),
        committed
    );

    // The manifest is current: same corpus, same split, same artifact hash.
    let dev_loo = leave_one_out(&cases, &sp.development(), &RECORDED_CONFIG);
    let manifest = manifest_json(&cases, &sp, &RECORDED_CONFIG, &model, &dev_loo);
    assert_eq!(
        manifest,
        fs::read_to_string(models_dir().join("local-decision-v1.manifest.json")).unwrap()
    );
    assert!(manifest.contains(&artifact_sha256(&model)));
    assert!(manifest.contains(&corpus_digest(&cases)));
    assert!(manifest.contains("\"schema\": \"chip.local-decision.v1\""));
    assert!(manifest.contains("\"feature_schema\": \"chip.decision-features.v1\""));
    assert!(manifest.contains("\"seed\": 42"));
}

#[test]
fn the_committed_report_matches_a_fresh_evaluation() {
    let cases = labeled_corpus();
    let (model, sp) = train_final(&cases, &RECORDED_CONFIG);
    assert_eq!(
        full_report(&cases, &sp, &RECORDED_CONFIG, &model),
        fs::read_to_string(models_dir().join("local-decision-v1.report.txt")).unwrap(),
        "regenerate with `cargo run -p chip-local-decision-train -- train`"
    );
}

#[test]
fn nothing_from_the_held_out_cases_reaches_the_model() {
    let cases = labeled_corpus();
    let (model, sp) = train_final(&cases, &RECORDED_CONFIG);
    let tokens_of = |indices: &[usize]| -> BTreeSet<Token> {
        indices
            .iter()
            .flat_map(|&i| cases[i].state.inputs.iter().map(|(n, v)| Token::of(n, v)))
            .collect()
    };
    let dev = tokens_of(&sp.development());
    let held = tokens_of(&sp.held_out);
    for token in &model.vocabulary().tokens {
        assert!(
            dev.contains(token),
            "{token:?} is in the vocabulary but in no development case"
        );
    }
    let only_held: Vec<_> = held.difference(&dev).collect();
    assert!(
        !only_held.is_empty(),
        "the held-out split must contain facts the model has never seen"
    );
    for token in only_held {
        assert!(
            !model.vocabulary().tokens.contains(token),
            "{token:?} leaked from the held-out split"
        );
    }
}

#[test]
fn zero_false_continues_everywhere_and_never_worse_than_the_baseline() {
    let cases = labeled_corpus();
    let (model, sp) = train_final(&cases, &RECORDED_CONFIG);

    let held_out: Vec<_> = sp.held_out.iter().map(|&i| cases[i].clone()).collect();
    let held = evaluate(Some(&model), &held_out);
    let dev_loo = leave_one_out(&cases, &sp.development(), &RECORDED_CONFIG);
    let all_loo = leave_one_out(&cases, &(0..32).collect::<Vec<_>>(), &RECORDED_CONFIG);
    let in_sample = evaluate(Some(&model), &cases);

    for (name, e) in [
        ("held-out", &held),
        ("loo dev", &dev_loo),
        ("loo all", &all_loo),
        ("in-sample", &in_sample),
    ] {
        println!(
            "{name}: additional safe local decisions {} (local_gain {}), learned false continues {}, guarded false continues {}",
            e.safe_local_gain(),
            e.local_gain(),
            e.learned.fp,
            e.guarded.fp
        );
        assert_eq!(
            e.learned.fp,
            0,
            "{name}: the raw learned model made a false continue: {:?}",
            e.unsafe_learned().iter().map(|c| &c.id).collect::<Vec<_>>()
        );
        assert_eq!(e.guarded.fp, 0, "{name}: guarded");
        assert_eq!(e.strict.fp, 0, "{name}: strict");
        assert!(
            e.guarded.tp >= e.deterministic.tp,
            "{name}: guarded never loses baseline coverage"
        );
        assert_eq!(
            e.local_gain(),
            e.safe_local_gain(),
            "{name}: with no false continue, local gain is safe gain"
        );
    }
}

#[test]
fn the_hidden_layer_experiment_is_recorded_as_rejected() {
    let cases = labeled_corpus();
    let sp = split(&cases, 42);
    let results = mlp::experiment(&cases, &sp.development());
    let safe_best = results
        .iter()
        .filter(|(_, r)| r.false_continues == 0)
        .map(|(_, r)| r.additional_safe)
        .max()
        .unwrap_or(0);
    assert_eq!(
        safe_best, 0,
        "if a hidden layer ever gains safely, revisit the decision to ship the linear model"
    );
    assert!(
        results.iter().any(|(_, r)| r.false_continues > 0),
        "the grid includes configurations that are unsafe"
    );
}

/// The grid search is slow in a debug build; run it with
/// `cargo test --release -p chip-local-decision-train -- --ignored`.
#[test]
#[ignore]
fn the_recorded_configuration_is_what_selection_chooses() {
    let cases = labeled_corpus();
    let sp = split(&cases, 42);
    let (chosen, _) = select(&cases, &sp.development(), &RECORDED_CONFIG);
    assert_eq!(chosen, RECORDED_CONFIG);
}
