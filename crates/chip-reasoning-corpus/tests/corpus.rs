//! The corpus is a contract: well-formed, deterministic, self-contained.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use chip_core::{EvidenceState, LocalReasoner, ReasoningError, ReasoningInput, TestLocalReasoner};
use chip_reasoning_corpus::{Category, Classification, Verdict, agreement, corpus, evaluate};

#[test]
fn the_corpus_has_a_reviewable_size_and_unique_ids() {
    let cases = corpus();
    assert!((20..=50).contains(&cases.len()), "{} cases", cases.len());
    let ids: HashSet<_> = cases.iter().map(|c| c.id).collect();
    assert_eq!(ids.len(), cases.len(), "ids must be unique");
    for case in &cases {
        assert!(!case.reason.trim().is_empty(), "{} needs a reason", case.id);
    }
}

#[test]
fn every_category_is_represented() {
    let cases = corpus();
    for category in Category::ALL {
        assert!(
            cases.iter().any(|c| c.category == category),
            "{}",
            category.name()
        );
    }
}

#[test]
fn the_corpus_is_deterministic() {
    assert_eq!(corpus(), corpus());
}

#[test]
fn valid_evidence_is_always_deterministic_and_continues() {
    for case in corpus()
        .iter()
        .filter(|c| c.input.evidence == EvidenceState::KnownValid)
    {
        assert_eq!(case.category, Category::Deterministic, "{}", case.id);
        assert_eq!(case.expected, Verdict::Continue, "{}", case.id);
    }
}

#[test]
fn escalation_required_cases_expect_escalate_and_classifications_follow_categories() {
    for case in corpus() {
        if case.category.classification() == Classification::EscalationRequired {
            assert_eq!(case.expected, Verdict::Escalate, "{}", case.id);
        }
    }
    assert_eq!(
        Category::Deterministic.classification(),
        Classification::Deterministic
    );
    assert_eq!(
        Category::LocalJudgment.classification(),
        Classification::LocalJudgment
    );
}

#[test]
fn local_judgment_cases_carry_the_facts_they_need() {
    for case in corpus()
        .iter()
        .filter(|c| c.category == Category::LocalJudgment)
    {
        assert!(
            !case.input.inputs.is_empty(),
            "{} must state its facts",
            case.id
        );
    }
}

#[test]
fn both_verdicts_are_expected_somewhere_so_always_answering_is_not_enough() {
    let cases = corpus();
    assert!(cases.iter().any(|c| c.expected == Verdict::Continue));
    assert!(cases.iter().any(|c| c.expected == Verdict::Escalate));
    let local: Vec<_> = cases
        .iter()
        .filter(|c| c.category == Category::LocalJudgment)
        .collect();
    assert!(local.iter().any(|c| c.expected == Verdict::Continue));
    assert!(local.iter().any(|c| c.expected == Verdict::Escalate));
}

#[test]
fn case_data_cannot_reach_the_environment() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let data = fs::read_to_string(dir.join("corpus.rs")).unwrap();
    let code: String = data
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        "std::fs",
        "std::env",
        "std::net",
        "std::process",
        "std::time",
        "Instant",
        "SystemTime",
        "static mut",
        "thread_local",
        "lazy",
        "OnceLock",
        "Mutex",
        "tokio",
    ] {
        assert!(
            !code.contains(forbidden),
            "corpus.rs must not reference {forbidden}"
        );
    }
    // The harness reaches nothing but a reasoner (and a clock for latency).
    let harness = fs::read_to_string(dir.join("evaluate.rs"))
        .unwrap()
        .to_lowercase();
    for forbidden in [
        "agent",
        "executor",
        "modelprovider",
        "evidence",
        "std::fs",
        "std::env",
        "std::net",
        "std::process",
        "tokio",
    ] {
        let code: String = harness
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains(forbidden),
            "evaluate.rs must not reference {forbidden}"
        );
    }
}

#[test]
fn the_deterministic_baseline_is_correct_on_what_it_should_be_and_never_unsafe() {
    let cases = corpus();
    let result = evaluate(&TestLocalReasoner::default(), &cases);
    assert_eq!(result.total, cases.len());
    assert_eq!(result.correct + result.incorrect, result.total);
    assert_eq!(result.reasoner_calls, cases.len());
    assert_eq!(result.continue_count + result.escalate_count, result.total);
    // It decides from evidence state alone: every deterministic and escalation-required
    // case is right, and its mistakes are needless escalations, never false continues.
    for case in &result.cases {
        if matches!(case.category, Category::Deterministic)
            || case.category.classification() == Classification::EscalationRequired
        {
            assert!(case.correct, "{} should be correct", case.case_id);
        }
    }
    assert_eq!(result.false_continues(), 0);
    assert!(
        result.needless_escalations() > 0,
        "the corpus must exercise judgment the baseline lacks"
    );
    assert!(result.mismatches().all(|c| c.is_needless_escalation()));
    assert_eq!(result.by_category()["deterministic"], (8, 8));
}

#[test]
fn mismatches_and_false_continues_are_surfaced() {
    struct AlwaysContinue;
    impl LocalReasoner for AlwaysContinue {
        fn reason(
            &self,
            _i: &ReasoningInput,
        ) -> Result<chip_core::LocalReasoningResult, ReasoningError> {
            Ok(chip_core::LocalReasoningResult::Continue {
                rationale: "yes".into(),
            })
        }
    }
    let result = evaluate(&AlwaysContinue, &corpus());
    assert!(result.false_continues() > 0, "always answering is unsafe");
    assert!(result.mismatches().count() > 0);
    assert!(result.accuracy() < 1.0);
}

#[test]
fn a_failing_reasoner_is_a_recorded_mismatch_not_a_panic() {
    struct Broken;
    impl LocalReasoner for Broken {
        fn reason(
            &self,
            _i: &ReasoningInput,
        ) -> Result<chip_core::LocalReasoningResult, ReasoningError> {
            Err(ReasoningError::Failed("boom".into()))
        }
    }
    let result = evaluate(&Broken, &corpus());
    assert_eq!(result.correct, 0);
    assert!(
        result
            .cases
            .iter()
            .all(|c| c.actual.is_none() && c.error.is_some())
    );
}

#[test]
fn evaluation_is_repeatable_and_agreement_is_checked_case_by_case() {
    let cases = corpus();
    let a = evaluate(&TestLocalReasoner::default(), &cases);
    let b = evaluate(&TestLocalReasoner::default(), &cases);
    assert!(agreement(&a, &b).is_empty());
    for (x, y) in a.cases.iter().zip(&b.cases) {
        assert_eq!(
            (x.case_id, x.actual, x.correct),
            (y.case_id, y.actual, y.correct)
        );
    }
    struct Flip;
    impl LocalReasoner for Flip {
        fn reason(
            &self,
            i: &ReasoningInput,
        ) -> Result<chip_core::LocalReasoningResult, ReasoningError> {
            TestLocalReasoner::default().reason(i).map(|r| match r {
                chip_core::LocalReasoningResult::Continue { rationale } => {
                    chip_core::LocalReasoningResult::Escalate { reason: rationale }
                }
                chip_core::LocalReasoningResult::Escalate { reason } => {
                    chip_core::LocalReasoningResult::Continue { rationale: reason }
                }
            })
        }
    }
    assert_eq!(agreement(&a, &evaluate(&Flip, &cases)).len(), cases.len());
}
