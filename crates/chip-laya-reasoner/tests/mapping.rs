#![cfg(feature = "laya")]
//! Adapter contract tests with a stand-in typed decider (no weights needed).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chip_core::{
    CapabilityId, EvidenceState, InputValue, LocalReasoner, LocalReasoningResult, ReasoningError,
    ReasoningInput,
};
use chip_laya_reasoner::{
    CONTINUE_LABEL, ESCALATE_LABEL, LayaProvenance, LayaReasoner, LayaVerdict, QUESTION_ID,
    STATE_SCHEMA, TypedDecider, encode_state, interpret, questions,
};
use indexmap::IndexMap;
use laya::LayaError;
use laya::agent::SystemOneResult;
use serde_json::{Value, json};

fn result(answers: Value) -> SystemOneResult {
    SystemOneResult {
        model: "stand-in".into(),
        answers: answers
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        input_tokens: 12,
        output_tokens: 0,
        routing: None,
    }
}

fn choice(label: &str, p_continue: f64) -> Value {
    json!({ QUESTION_ID: {
        "type": "choice", "choice": label,
        "probabilities": { CONTINUE_LABEL: p_continue, ESCALATE_LABEL: 1.0 - p_continue },
        "confidence": 0.8, "answer_confidence": 0.9, "action": { "act_probability": 0.5 }
    }})
}

struct Fake {
    answers: Value,
    fail: bool,
    calls: AtomicUsize,
    seen: Mutex<Vec<(Value, IndexMap<String, Value>)>>,
}

impl Fake {
    fn new(answers: Value) -> Arc<Self> {
        Arc::new(Self {
            answers,
            fail: false,
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        })
    }
}

struct Shared(Arc<Fake>);

impl TypedDecider for Shared {
    fn decide(
        &self,
        state: &Value,
        questions: &IndexMap<String, Value>,
    ) -> Result<SystemOneResult, LayaError> {
        let fake = &self.0;
        fake.calls.fetch_add(1, Ordering::SeqCst);
        fake.seen
            .lock()
            .unwrap()
            .push((state.clone(), questions.clone()));
        if fake.fail {
            return Err(LayaError::Model("boom".into()));
        }
        Ok(result(fake.answers.clone()))
    }
}

fn provenance() -> LayaProvenance {
    LayaProvenance {
        model: Some("stand-in".into()),
        path: PathBuf::from("/m"),
        backend: "candle-cpu",
        upstream_version: "x",
    }
}

fn reasoner(fake: &Arc<Fake>) -> LayaReasoner {
    LayaReasoner::from_decider(Box::new(Shared(fake.clone())), provenance())
}

fn input(evidence: EvidenceState) -> ReasoningInput {
    ReasoningInput {
        capability: CapabilityId::new("deploy.service").unwrap(),
        inputs: BTreeMap::from([
            ("service".to_string(), InputValue::Text("api".into())),
            ("replicas".to_string(), InputValue::Integer(3)),
            ("dry_run".to_string(), InputValue::Bool(true)),
        ]),
        evidence,
    }
}

#[test]
fn the_state_is_stable_structured_json_built_only_from_the_input() {
    let state = encode_state(&input(EvidenceState::KnownStale));
    assert_eq!(
        state,
        json!({"schema": STATE_SCHEMA, "capability": "deploy.service",
               "inputs": {"dry_run": true, "replicas": 3, "service": "api"}, "evidence_state": "stale"})
    );
    // Field order is fixed and the encoding is byte-identical every time.
    let text = serde_json::to_string(&state).unwrap();
    assert!(
        text.starts_with(
            "{\"schema\":\"chip.reasoning.v1\",\"capability\":\"deploy.service\",\"inputs\":{"
        ),
        "{text}"
    );
    assert!(text.ends_with("\"evidence_state\":\"stale\"}"));
    for _ in 0..5 {
        assert_eq!(
            serde_json::to_string(&encode_state(&input(EvidenceState::KnownStale))).unwrap(),
            text
        );
    }
    assert_eq!(
        encode_state(&input(EvidenceState::KnownValid))["evidence_state"],
        "valid"
    );
    assert_eq!(
        encode_state(&input(EvidenceState::Unknown))["evidence_state"],
        "unknown"
    );
}

#[test]
fn exactly_one_typed_choice_question_is_asked() {
    let q = questions();
    assert_eq!(q.len(), 1);
    let question = &q[QUESTION_ID];
    assert_eq!(question["type"], "choice");
    let labels: Vec<_> = question["criteria"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(labels, [CONTINUE_LABEL, ESCALATE_LABEL]);
    assert!(question["instructions"].as_str().unwrap().len() > 20);
}

#[test]
fn laya_receives_the_encoded_state_and_the_question_and_nothing_else() {
    let fake = Fake::new(choice("CONTINUE", 0.9));
    reasoner(&fake)
        .reason(&input(EvidenceState::KnownStale))
        .unwrap();
    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen[0].0, encode_state(&input(EvidenceState::KnownStale)));
    assert_eq!(seen[0].1, questions());
}

#[test]
fn continue_and_escalate_map_and_every_other_answer_fails_closed() {
    let verdict = |answers: Value| {
        let fake = Fake::new(answers);
        reasoner(&fake).reason(&input(EvidenceState::Unknown))
    };
    assert!(matches!(
        verdict(choice("CONTINUE", 0.9)),
        Ok(LocalReasoningResult::Continue { .. })
    ));
    assert!(matches!(
        verdict(choice("ESCALATE", 0.1)),
        Ok(LocalReasoningResult::Escalate { .. })
    ));

    let mut bad: Vec<Value> = vec![
        choice("continue", 0.9),
        choice("CONTINUE because", 0.9),
        choice("run rm -rf /", 0.9),
        choice("ESCALATE", 0.9), // contradicts its own probabilities
        json!({}),
        json!({ "other": choice("CONTINUE", 0.9)[QUESTION_ID].clone() }),
        json!({ QUESTION_ID: {"type": "noul", "noul": 0.9, "confidence": 0.9, "answer_confidence": 0.9, "action": {"act_probability": 0.5}} }),
        json!({ QUESTION_ID: {"type": "choice", "probabilities": {"CONTINUE": 0.5, "ESCALATE": 0.5}, "confidence": 1, "answer_confidence": 1, "action": {"act_probability": 0.5}} }),
    ];
    let mut two = choice("CONTINUE", 0.9);
    two.as_object_mut()
        .unwrap()
        .insert("extra".into(), choice("CONTINUE", 0.9)[QUESTION_ID].clone());
    bad.push(two);
    let mut missing_prob = choice("CONTINUE", 0.9);
    missing_prob[QUESTION_ID]["probabilities"] = json!({ "CONTINUE": 1.0 });
    bad.push(missing_prob);
    let mut out_of_range = choice("CONTINUE", 0.9);
    out_of_range[QUESTION_ID]["probabilities"] = json!({ "CONTINUE": 1.5, "ESCALATE": -0.5 });
    bad.push(out_of_range);
    let mut no_confidence = choice("CONTINUE", 0.9);
    no_confidence[QUESTION_ID]
        .as_object_mut()
        .unwrap()
        .remove("confidence");
    bad.push(no_confidence);
    for answers in bad {
        let shown = answers.to_string();
        assert!(
            matches!(verdict(answers), Err(ReasoningError::Failed(_))),
            "accepted: {shown}"
        );
    }

    let fake = Arc::new(Fake {
        answers: choice("CONTINUE", 0.9),
        fail: true,
        calls: AtomicUsize::new(0),
        seen: Mutex::new(vec![]),
    });
    assert!(matches!(
        reasoner(&fake).reason(&input(EvidenceState::Unknown)),
        Err(ReasoningError::Failed(_))
    ));
}

#[test]
fn confidence_and_probabilities_are_recorded_and_never_change_the_verdict() {
    let fake = Fake::new(choice("CONTINUE", 0.97));
    let judgment = reasoner(&fake)
        .judge(&input(EvidenceState::KnownStale))
        .unwrap();
    assert_eq!(judgment.verdict, LayaVerdict::Continue);
    assert_eq!(judgment.confidence, 0.8);
    assert_eq!(judgment.answer_confidence, 0.9);
    assert_eq!(judgment.act_probability, 0.5);
    assert!((judgment.probabilities[CONTINUE_LABEL] - 0.97).abs() < 1e-6);
    assert_eq!(judgment.input_tokens, 12);
    // A barely-more-likely CONTINUE and a near-certain ESCALATE are still just those verdicts.
    assert!(matches!(
        reasoner(&Fake::new(choice("CONTINUE", 0.51))).reason(&input(EvidenceState::Unknown)),
        Ok(LocalReasoningResult::Continue { .. })
    ));
    assert!(matches!(
        reasoner(&Fake::new(choice("ESCALATE", 0.01))).reason(&input(EvidenceState::Unknown)),
        Ok(LocalReasoningResult::Escalate { .. })
    ));
    // The decision contract itself carries no confidence.
    let result = reasoner(&fake)
        .reason(&input(EvidenceState::KnownStale))
        .unwrap();
    assert!(!format!("{result:?}").contains("0.97"));
}

#[test]
fn interpret_is_the_single_place_that_reads_laya() {
    let ok = interpret(&result(choice("ESCALATE", 0.2)), Duration::from_millis(1)).unwrap();
    assert_eq!(ok.verdict, LayaVerdict::Escalate);
    assert_eq!(ok.latency, Duration::from_millis(1));
}

#[test]
fn provenance_is_observable_and_separate_from_the_decision() {
    let fake = Fake::new(choice("CONTINUE", 0.9));
    let r = reasoner(&fake);
    assert_eq!(r.provenance().path, PathBuf::from("/m"));
    assert!(r.provenance().to_string().contains("stand-in"));
    assert!(r.provenance().to_string().contains("candle-cpu"));
}
