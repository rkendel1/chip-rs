use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use chip_core::CapabilityId;
use chip_decision_corpus::{
    CAPABILITIES, CORPUS_SCHEMA, Corpus, CorpusCase, FactValue, GENERATOR_VERSION, GeneratorConfig,
    HELD_OUT_CAPABILITIES, Split, UNKNOWN_SPELLINGS, corpus_v1, generate,
};

/// The digest of the corpus generator version 1 produces. If this fails, the generator changed:
/// either revert, or bump `GENERATOR_VERSION`, regenerate the committed files and update this.
const PINNED_DIGEST: &str = "5f128c3f13ab3fbd07d3a4170516c95fe70d13ec0da5a482b95fe27a2d74e012";

fn dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

#[test]
fn generation_is_deterministic_and_versioned() {
    let a = corpus_v1();
    let b = corpus_v1();
    assert_eq!(a, b);
    assert_eq!(a.to_jsonl(), b.to_jsonl());
    assert_eq!(
        a.digest(),
        PINNED_DIGEST,
        "the generator's output changed; see the comment on PINNED_DIGEST"
    );
    assert_eq!(a.generator, GENERATOR_VERSION);
    assert_eq!(GENERATOR_VERSION, "chip.decision-corpus-gen.v1");
    assert!(
        a.to_jsonl()
            .starts_with(&format!("{{\"schema\":\"{CORPUS_SCHEMA}\""))
    );

    // A different seed is a different corpus; a different configuration, too.
    let other_seed = generate(&GeneratorConfig {
        seed: 7,
        ..GeneratorConfig::V1
    });
    assert_ne!(other_seed.digest(), a.digest());
    assert_eq!(
        other_seed.cases.len(),
        a.cases.len(),
        "the structure is the same; the contexts differ"
    );
}

#[test]
fn the_committed_corpus_is_exactly_what_the_generator_produces() {
    let corpus = corpus_v1();
    assert_eq!(
        fs::read_to_string(dir().join("decision-corpus-v1.jsonl")).unwrap(),
        corpus.to_jsonl()
    );
    let manifest = fs::read_to_string(dir().join("decision-corpus-v1.manifest.json")).unwrap();
    assert!(manifest.contains(&corpus.digest()));
    assert!(manifest.contains(&format!("\"cases\": {}", corpus.cases.len())));
    assert!(manifest.contains(GENERATOR_VERSION) && manifest.contains(CORPUS_SCHEMA));
}

#[test]
fn the_corpus_is_substantial_and_ids_are_unique_and_ordered() {
    let corpus = corpus_v1();
    assert!(corpus.cases.len() >= 500, "{} cases", corpus.cases.len());
    let ids: Vec<&str> = corpus.cases.iter().map(|c| c.case_id.as_str()).collect();
    let unique: BTreeSet<&&str> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len());
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            *id,
            format!("dc-{:04}", i + 1),
            "ids are sequential in corpus order"
        );
    }
}

const FACT_NAMES: [&str; 16] = [
    "prerequisites_met",
    "last_outcome",
    "requires_approval",
    "approval_granted",
    "approval_revoked",
    "read_only",
    "idempotent",
    "sources_agree",
    "reported_failure",
    "state_changed",
    "escalate_requested",
    "policy_ambiguous",
    "target",
    "retries",
    "dry_run",
    "priority",
];

#[test]
fn every_case_is_valid() {
    for c in &corpus_v1().cases {
        CapabilityId::new(c.capability.clone()).unwrap();
        assert!(CAPABILITIES.contains(&c.capability.as_str()));
        assert!(!c.rationale.is_empty(), "{}", c.case_id);
        assert!(
            c.rationale.starts_with(if c.expected_continue {
                "Continue:"
            } else {
                "Escalate:"
            }),
            "{}",
            c.case_id
        );
        for (name, value) in &c.facts {
            assert!(
                FACT_NAMES.contains(&name.as_str()),
                "{}: unknown fact {name}",
                c.case_id
            );
            match (name.as_str(), value) {
                ("retries", FactValue::Int(n)) => assert!((0..3).contains(n)),
                ("target" | "priority", FactValue::Text(_)) => {}
                ("last_outcome", FactValue::Text(t)) => {
                    assert!(
                        ["success", "failure"].contains(&t.as_str())
                            || UNKNOWN_SPELLINGS.contains(&t.as_str())
                    )
                }
                (_, FactValue::Bool(_)) => {}
                (_, FactValue::Text(t)) => assert!(
                    UNKNOWN_SPELLINGS.contains(&t.as_str()),
                    "{}: {name}={t}",
                    c.case_id
                ),
                (n, v) => panic!("{}: {n} has an unexpected value {v:?}", c.case_id),
            }
        }
        // The state a model sees carries no metadata.
        let state = c.decision_state();
        assert_eq!(state.inputs.len(), c.facts.len());
    }
}

/// An independent statement of the documented labelling rules.
fn oracle(c: &CorpusCase) -> bool {
    use chip_core::{EvidenceState::*, ImpactState::*};
    let f = &c.facts;
    let is = |k: &str, v: bool| f.get(k) == Some(&FactValue::Bool(v));
    let unknown = |k: &str| matches!(f.get(k), Some(FactValue::Text(t)) if UNKNOWN_SPELLINGS.contains(&t.as_str()));
    if c.evidence == KnownValid {
        return c.impact == Unchanged;
    }
    if c.impact == Impacted {
        return false;
    }
    if is("sources_agree", false)
        || is("reported_failure", true)
        || is("state_changed", true)
        || is("escalate_requested", true)
        || is("policy_ambiguous", true)
        || is("approval_revoked", true)
    {
        return false;
    }
    for k in [
        "prerequisites_met",
        "last_outcome",
        "approval_granted",
        "read_only",
        "idempotent",
    ] {
        if unknown(k) {
            return false;
        }
    }
    if is("approval_granted", false)
        || (is("requires_approval", true) && !is("approval_granted", true))
    {
        return false;
    }
    match c.evidence {
        KnownStale => {
            is("prerequisites_met", true)
                && !matches!(f.get("last_outcome"), Some(FactValue::Text(t)) if t == "failure")
        }
        Unknown => is("prerequisites_met", true) && is("read_only", true) && is("idempotent", true),
        KnownValid => unreachable!(),
    }
}

#[test]
fn every_label_follows_the_documented_rules() {
    for c in &corpus_v1().cases {
        assert_eq!(
            oracle(c),
            c.expected_continue,
            "{} ({}): {}",
            c.case_id,
            c.pattern_id,
            c.rationale
        );
    }
}

#[test]
fn no_two_cases_are_the_same_decision_state() {
    // The graph token is opaque and ignored, so it does not make two cases different.
    let mut seen = BTreeSet::new();
    for c in &corpus_v1().cases {
        let key = format!(
            "{}|{:?}|{:?}|{:?}",
            c.capability, c.evidence, c.impact, c.facts
        );
        assert!(seen.insert(key), "{} duplicates an earlier case", c.case_id);
    }
}

#[test]
fn the_corpus_has_the_required_structure() {
    let corpus = corpus_v1();
    let families: BTreeSet<&str> = corpus.cases.iter().map(|c| c.family.name()).collect();
    assert_eq!(families.len(), 6);
    let any = |f: &dyn Fn(&CorpusCase) -> bool| corpus.cases.iter().any(|c| f(c));
    assert!(
        any(&|c| c.tags.hard_negative)
            && any(&|c| c.tags.conjunction)
            && any(&|c| c.tags.conflict)
            && any(&|c| c.tags.unknown)
    );
    let positives = corpus.cases.iter().filter(|c| c.expected_continue).count();
    assert!(positives > 100 && positives < corpus.cases.len() / 2);

    // Each positive pattern has near-miss negatives, and each fact recurs across capabilities.
    let by_pattern: BTreeMap<&str, Vec<&CorpusCase>> =
        corpus.cases.iter().fold(BTreeMap::new(), |mut m, c| {
            m.entry(c.pattern_id.as_str()).or_default().push(c);
            m
        });
    for (pattern, cases) in &by_pattern {
        let capabilities: BTreeSet<&str> = cases.iter().map(|c| c.capability.as_str()).collect();
        assert!(
            capabilities.len() >= 3,
            "{pattern} appears in only {} capabilities",
            capabilities.len()
        );
        let graphs: BTreeSet<_> = cases.iter().map(|c| c.graph_state).collect();
        assert_eq!(
            graphs.len(),
            cases.len(),
            "{pattern}: graph tokens differ per case"
        );
    }
    let hard_negative_patterns = by_pattern
        .iter()
        .filter(|(_, v)| v[0].tags.hard_negative)
        .count();
    assert!(hard_negative_patterns >= 30);
    // Unknown is its own value, in several spellings, and never the same as false or absence.
    for spelling in UNKNOWN_SPELLINGS {
        assert!(
            any(&|c| c
                .facts
                .values()
                .any(|v| *v == FactValue::Text(spelling.to_string()))),
            "{spelling}"
        );
    }
    assert!(any(
        &|c| c.facts.get("approval_granted") == Some(&FactValue::Bool(false))
    ));
    assert!(any(&|c| c.facts.get("requires_approval")
        == Some(&FactValue::Bool(true))
        && !c.facts.contains_key("approval_granted")));
}

#[test]
fn unknown_never_justifies_continuing_except_where_valid_evidence_already_does() {
    for c in &corpus_v1().cases {
        let critical_unknown = c.facts.iter().any(|(k, v)| {
            matches!(v, FactValue::Text(t) if UNKNOWN_SPELLINGS.contains(&t.as_str()))
                && !["target", "priority"].contains(&k.as_str())
        });
        if critical_unknown && c.evidence != chip_core::EvidenceState::KnownValid {
            assert!(!c.expected_continue, "{}", c.case_id);
        }
    }
}

#[test]
fn nothing_about_a_case_identifies_its_label() {
    let corpus: Corpus = corpus_v1();
    let overall = corpus.cases.iter().filter(|c| c.expected_continue).count() as f64
        / corpus.cases.len() as f64;
    let rate = |cases: &[&CorpusCase]| {
        cases.iter().filter(|c| c.expected_continue).count() as f64 / cases.len() as f64
    };

    // Capability ids.
    for capability in CAPABILITIES {
        let cases: Vec<_> = corpus
            .cases
            .iter()
            .filter(|c| c.capability == capability)
            .collect();
        assert!(cases.len() >= 30, "{capability}: {} cases", cases.len());
        assert!(
            (rate(&cases) - overall).abs() < 0.15,
            "{capability}: {:.2} vs {overall:.2}",
            rate(&cases)
        );
    }
    // Case ids and corpus order, in quarters.
    let quarters = |sorted: Vec<&CorpusCase>| {
        let n = sorted.len();
        (0..4)
            .map(|q| rate(&sorted[q * n / 4..(q + 1) * n / 4]))
            .collect::<Vec<_>>()
    };
    let in_order: Vec<_> = corpus.cases.iter().collect();
    for r in quarters(in_order) {
        assert!(
            (r - overall).abs() < 0.08,
            "id order predicts the label: {r:.2} vs {overall:.2}"
        );
    }
    // Graph tokens.
    let mut by_graph: Vec<_> = corpus.cases.iter().collect();
    by_graph.sort_by_key(|c| c.graph_state);
    for r in quarters(by_graph) {
        assert!(
            (r - overall).abs() < 0.08,
            "graph tokens predict the label: {r:.2} vs {overall:.2}"
        );
    }
    // Ids and rationales say nothing a token could pick up: ids are bare numbers.
    assert!(
        corpus
            .cases
            .iter()
            .all(|c| c.case_id.chars().filter(|ch| ch.is_alphabetic()).count() == 2)
    );
}

#[test]
fn the_splits_do_not_leak() {
    let corpus = corpus_v1();
    let train: Vec<_> = corpus
        .cases
        .iter()
        .filter(|c| c.split == Split::Train)
        .collect();
    let held: Vec<_> = corpus
        .cases
        .iter()
        .filter(|c| c.split == Split::HeldOut)
        .collect();
    assert!(train.len() > 200 && held.len() > 200);

    // Split and flags agree; a trained-on case has no holdout flag.
    for c in &corpus.cases {
        assert_eq!(c.holdouts.any(), c.split == Split::HeldOut, "{}", c.case_id);
        if c.split != Split::HeldOut {
            assert!(!c.holdouts.any());
        }
    }
    let ids = |v: &[&CorpusCase]| v.iter().map(|c| c.case_id.clone()).collect::<BTreeSet<_>>();
    assert!(ids(&train).is_disjoint(&ids(&held)));

    // Pattern holdout: a pattern is withheld whole, in every context, so no variant crosses.
    let held_patterns: BTreeSet<&str> = corpus
        .cases
        .iter()
        .filter(|c| c.holdouts.pattern)
        .map(|c| c.pattern_id.as_str())
        .collect();
    for c in &corpus.cases {
        assert_eq!(
            c.holdouts.pattern,
            held_patterns.contains(c.pattern_id.as_str()),
            "{}",
            c.case_id
        );
    }
    assert!(
        train
            .iter()
            .all(|c| !held_patterns.contains(c.pattern_id.as_str()))
    );

    // Context holdout: a context is withheld whole.
    let held_contexts: BTreeSet<&str> = corpus
        .cases
        .iter()
        .filter(|c| c.holdouts.context)
        .map(|c| c.context_id.as_str())
        .collect();
    assert!(!held_contexts.is_empty());
    assert!(
        train
            .iter()
            .all(|c| !held_contexts.contains(c.context_id.as_str()))
    );

    // Capability holdout.
    assert!(
        train
            .iter()
            .all(|c| !HELD_OUT_CAPABILITIES.contains(&c.capability.as_str()))
    );
    assert!(held.iter().any(|c| c.holdouts.capability));

    // Hard negatives are held out only as whole patterns or by context, never as stray variants of
    // a pattern still being trained on... unless by the random case draw, which is flagged.
    for c in held.iter().filter(|c| c.tags.hard_negative) {
        assert!(c.holdouts.any());
    }

    // A withheld pattern is a novel *combination*, not a novel fact: every (fact, value) it uses is
    // used by some pattern kept in training. Compared as shapes, spellings of unknown merged.
    let shape = |c: &CorpusCase| -> BTreeSet<String> {
        let mut s: BTreeSet<String> = c
            .facts
            .iter()
            .filter(|(k, _)| !["target", "retries", "dry_run", "priority"].contains(&k.as_str()))
            .map(|(k, v)| match v {
                FactValue::Text(t) if UNKNOWN_SPELLINGS.contains(&t.as_str()) => {
                    format!("{k}=unknown")
                }
                v => format!("{k}={v:?}"),
            })
            .collect();
        s.insert(format!("evidence={:?}", c.evidence));
        s.insert(format!("impact={:?}", c.impact));
        s
    };
    let trained_shapes: BTreeSet<String> = train.iter().flat_map(|c| shape(c)).collect();
    for c in corpus.cases.iter().filter(|c| c.holdouts.pattern) {
        for part in shape(c) {
            assert!(
                trained_shapes.contains(&part),
                "{} uses {part}, which training never sees",
                c.pattern_id
            );
        }
    }
    // Every pattern exists, and the positive ones are not all withheld.
    assert!(
        train
            .iter()
            .any(|c| c.expected_continue && c.family.name() == "approval")
    );
}
