mod common;

use chip_core::{EvidenceState, ImpactState};
use chip_local_decision::{
    Decision, LocalDecider, LocalDecisionModel, PolicyMode, deterministic_decision,
};
use common::*;

const MODES: [PolicyMode; 4] = [
    PolicyMode::Deterministic,
    PolicyMode::Learned,
    PolicyMode::LearnedGuarded,
    PolicyMode::LearnedStrict,
];

#[test]
fn the_deterministic_policy_is_unchanged() {
    use chip_wasm_decision_host::decide_native;
    for s in all_states() {
        let expected = match (s.evidence_state, s.impact) {
            (EvidenceState::KnownValid, ImpactState::Unchanged) => Decision::Continue,
            _ => Decision::Escalate,
        };
        assert_eq!(deterministic_decision(&s), expected);
        assert_eq!(deterministic_decision(&s), decide_native(&s).decision);
        assert_eq!(
            deterministic_decision(&s),
            chip_wasm_decision::decide_bytes(&s.canonical_bytes())
                .unwrap()
                .decision
        );
        let decider = LocalDecider::new(Some(always_continue()), PolicyMode::Deterministic);
        assert_eq!(
            decider.decide(&s).decision,
            expected,
            "Deterministic mode ignores the model"
        );
    }
}

#[test]
fn learned_raw_and_guarded_decisions_are_both_reported() {
    let decider = LocalDecider::new(Some(always_continue()), PolicyMode::Learned);
    let stale_impacted = state(
        "tests.run",
        EvidenceState::KnownStale,
        ImpactState::Impacted,
    );
    let outcome = decider.decide(&stale_impacted);
    assert_eq!(
        outcome.learned_raw,
        Some(Decision::Continue),
        "the unsafe raw answer is visible"
    );
    assert_eq!(
        outcome.decision,
        Decision::Continue,
        "Learned uses it directly (measurement only)"
    );
    assert_eq!(outcome.guarded, Decision::Escalate, "the guard refuses it");
    assert_eq!(outcome.strict, Decision::Escalate);
    assert_eq!(outcome.deterministic, Decision::Escalate);
}

#[test]
fn the_guard_never_lets_a_learned_continue_past_an_impacted_capability() {
    // Worst case: a model that says Continue to everything.
    for mode in MODES {
        let decider = LocalDecider::new(Some(always_continue()), mode);
        for s in all_states() {
            let outcome = decider.decide(&s);
            if s.impact == ImpactState::Impacted {
                assert_eq!(outcome.guarded, Decision::Escalate, "{s:?}");
                assert_eq!(outcome.strict, Decision::Escalate);
            }
            // Strict is a subset of the baseline; guarded is a superset of it.
            if outcome.strict == Decision::Continue {
                assert_eq!(outcome.deterministic, Decision::Continue);
            }
            if outcome.deterministic == Decision::Continue {
                assert_eq!(
                    outcome.guarded,
                    Decision::Continue,
                    "the baseline's Continue is never weakened"
                );
            }
        }
    }
}

#[test]
fn a_model_that_never_continues_cannot_remove_baseline_coverage_when_guarded() {
    let decider = LocalDecider::new(Some(never_continue()), PolicyMode::LearnedGuarded);
    for s in all_states() {
        assert_eq!(decider.decide(&s).decision, deterministic_decision(&s));
    }
}

#[test]
fn disabling_the_learned_model_restores_deterministic_behavior_in_every_mode() {
    for mode in MODES {
        let mut decider = LocalDecider::new(Some(always_continue()), mode);
        assert!(decider.disable_model().is_some());
        assert!(decider.model().is_none());
        for s in all_states() {
            let outcome = decider.decide(&s);
            assert_eq!(outcome.decision, deterministic_decision(&s), "{mode:?}");
            assert_eq!(outcome.learned_raw, None);
            assert_eq!(outcome.guarded, deterministic_decision(&s));
        }
        let none = LocalDecider::new(None, mode);
        for s in all_states() {
            assert_eq!(none.decide(&s).decision, deterministic_decision(&s));
        }
    }
}

#[test]
fn the_embedded_model_is_a_valid_decider_in_every_mode() {
    let model = LocalDecisionModel::embedded().unwrap();
    for mode in MODES {
        let decider = LocalDecider::new(Some(model.clone()), mode);
        for s in all_states() {
            let outcome = decider.decide(&s);
            // Whatever the model says, an impacted capability is never continued by guarded.
            if s.impact == ImpactState::Impacted {
                assert_eq!(outcome.guarded, Decision::Escalate);
            }
        }
    }
}
