//! Replays the operational decision corpus through the Rust baseline and the
//! WASM reasoner, and checks that the two agree. Offline and deterministic.

use std::sync::atomic::{AtomicUsize, Ordering};

use chip_core::{
    LocalReasoner, LocalReasoningResult, ReasoningError, ReasoningInput, TestLocalReasoner,
};
use chip_reasoning_corpus::{
    Disagreement, EvaluationResult, ReasoningCase, agreement, corpus, evaluate,
};
use chip_wasm_reasoner::{WasmLocalReasoner, benchmark_fixture};

use crate::benchmark::fmt;

/// The native local model's part of the report. Unavailable infrastructure is
/// "skipped", never a model failure.
pub enum NativeOutcome {
    Skipped(String),
    Evaluated {
        result: EvaluationResult,
        description: String,
        init: std::time::Duration,
        samples: Vec<crate::native::Sample>,
    },
}

pub struct CorpusReport {
    pub native: NativeOutcome,
    pub cases: Vec<ReasoningCase>,
    pub rust: EvaluationResult,
    pub wasm: EvaluationResult,
    pub disagreements: Vec<Disagreement>,
}

struct Counting<'a> {
    inner: &'a dyn LocalReasoner,
    calls: AtomicUsize,
}

impl LocalReasoner for Counting<'_> {
    fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.reason(input)
    }
}

/// Both reasoners receive exactly the same structured cases.
pub fn run() -> Result<CorpusReport, String> {
    let cases = corpus();
    let rust_reasoner = TestLocalReasoner::default();
    let wasm_reasoner =
        WasmLocalReasoner::from_bytes(&benchmark_fixture()).map_err(|e| e.to_string())?;
    let rust_counter = Counting {
        inner: &rust_reasoner,
        calls: AtomicUsize::new(0),
    };
    let wasm_counter = Counting {
        inner: &wasm_reasoner,
        calls: AtomicUsize::new(0),
    };
    let rust = evaluate(&rust_counter, &cases);
    let wasm = evaluate(&wasm_counter, &cases);
    for (name, result, counter) in [
        ("Rust", &rust, &rust_counter),
        ("WASM", &wasm, &wasm_counter),
    ] {
        if counter.calls.load(Ordering::SeqCst) != cases.len()
            || result.reasoner_calls != cases.len()
        {
            return Err(format!(
                "{name}: expected exactly one reasoner call per case"
            ));
        }
    }
    let disagreements = agreement(&rust, &wasm);
    // The same evaluator and the same corpus; the model is just another reasoner.
    let native = match crate::native::load() {
        Err(reason) => NativeOutcome::Skipped(reason),
        Ok(model) => {
            let result = evaluate(&*model.reasoner, &cases);
            NativeOutcome::Evaluated {
                result,
                description: model.description.clone(),
                init: model.init,
                samples: model.take_samples(),
            }
        }
    };
    Ok(CorpusReport {
        native,
        cases,
        rust,
        wasm,
        disagreements,
    })
}

fn block(name: &str, r: &EvaluationResult) -> String {
    format!(
        "{name}:\n  Correct: {}\n  Incorrect: {}\n  Accuracy: {:.1}%\n  False continues: {}\n  Needless escalations: {}\n  Continue / Escalate: {} / {}\n  Median latency: {}\n\n",
        r.correct,
        r.incorrect,
        r.accuracy() * 100.0,
        r.false_continues(),
        r.needless_escalations(),
        r.continue_count,
        r.escalate_count,
        fmt(r.median_latency())
    )
}

fn native_block(report: &CorpusReport) -> String {
    match &report.native {
        NativeOutcome::Skipped(reason) => format!("Native model: skipped — {reason}\n\n"),
        NativeOutcome::Evaluated {
            result,
            description,
            init,
            samples,
        } => {
            let mut out = block("Native model", result);
            out += &format!("  Model: {description}\n  Initialization: {}\n", fmt(*init));
            let improvement = result.safe_improvement_over(&report.rust);
            out += &match improvement {
                Some(n) => format!(
                    "  Safe improvement over Rust baseline: {n} needless escalation(s) recovered, 0 false continues\n"
                ),
                None => format!(
                    "  Safe improvement over Rust baseline: none ({} false continue(s))\n",
                    result.false_continues()
                ),
            };
            if !samples.is_empty() {
                let mean = samples.iter().map(|s| s.confidence).sum::<f64>() / samples.len() as f64;
                out += &format!("  Mean confidence (observational only): {mean:.3}\n");
            }
            out += "\n";
            out += &mismatches(result);
            out
        }
    }
}

fn mismatches(r: &EvaluationResult) -> String {
    let mut out = String::new();
    for c in r.mismatches() {
        out += &format!(
            "Case: {}\nExpected: {}\nActual: {}\nClassification: {}\n\n",
            c.case_id,
            c.expected.name(),
            c.actual.map(|v| v.name()).unwrap_or("ERROR"),
            c.category.name()
        );
    }
    out
}

pub fn render(report: &CorpusReport) -> String {
    let mut out = format!("Reasoning Corpus\n\nCases: {}\n\n", report.cases.len());
    out += &block("Rust", &report.rust);
    out += &block("WASM", &report.wasm);
    out += &native_block(report);
    out += &format!(
        "Rust/WASM agreement: {}/{}\n\n",
        report.cases.len() - report.disagreements.len(),
        report.cases.len()
    );
    out += "By category (Rust baseline, correct/total):\n";
    for (category, (correct, total)) in report.rust.by_category() {
        out += &format!("  {category}: {correct}/{total}\n");
    }
    out += "\nRust baseline mismatches:\n";
    let rust_mismatches = mismatches(&report.rust);
    out += if rust_mismatches.is_empty() {
        "  none\n\n"
    } else {
        &rust_mismatches
    };
    if mismatches(&report.wasm) == rust_mismatches {
        out += "WASM mismatches: identical to Rust\n\n";
    } else {
        out += "WASM mismatches:\n";
        out += &mismatches(&report.wasm);
    }
    for d in &report.disagreements {
        out += &format!(
            "DISAGREEMENT {}: Rust {:?} vs WASM {:?}\n",
            d.case_id, d.first, d.second
        );
    }
    out += "Model calls: 0\nExecutions: 0\nEvidence writes: 0\n(the replay harness has no access to a model, an executor or evidence)\n";
    out += &format!(
        "\nReasoner calls: Rust {}, WASM {}\n",
        report.rust.reasoner_calls, report.wasm.reasoner_calls
    );
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chip_core::{
        Agent, Assessment, CapabilityRequest, EvidenceLookup, EvidenceState, ExecutionId, Executor,
        Observation, ObservationKind, StateToken,
    };
    use chip_core::{ExecutionError, ExecutionRequest, ExecutionResult, ExecutionStatus};
    use chip_reasoning_corpus::Verdict;
    use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse};

    use super::*;

    #[test]
    fn rust_and_wasm_agree_on_every_case() {
        let report = run().unwrap();
        assert!(
            report.disagreements.is_empty(),
            "{:?}",
            report.disagreements
        );
        for (r, w) in report.rust.cases.iter().zip(&report.wasm.cases) {
            assert_eq!(r.case_id, w.case_id);
            assert_eq!(r.actual, w.actual, "{}", r.case_id);
            assert!(w.error.is_none(), "{}: {:?}", w.case_id, w.error);
        }
        assert_eq!(report.rust.correct, report.wasm.correct);
    }

    #[test]
    fn the_report_surfaces_mismatches_and_states_its_claims() {
        let text = render(&run().unwrap());
        assert!(
            text.contains("Reasoning Corpus") && text.contains("Rust/WASM agreement: 32/32"),
            "{text}"
        );
        assert!(
            text.contains(
                "Case: lj-01\nExpected: CONTINUE\nActual: ESCALATE\nClassification: local_judgment"
            ),
            "{text}"
        );
        assert!(text.contains("False continues: 0") && text.contains("Model calls: 0"));
        assert!(!text.contains("DISAGREEMENT"));
    }

    /// A candidate local model, as a plain reasoner: continues exactly where the corpus
    /// says it should (the best possible safe model).
    struct Oracle;

    impl LocalReasoner for Oracle {
        fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
            let expected = corpus()
                .into_iter()
                .find(|c| c.input == *input)
                .map(|c| c.expected);
            Ok(match expected {
                Some(Verdict::Continue) => LocalReasoningResult::Continue {
                    rationale: "o".into(),
                },
                _ => LocalReasoningResult::Escalate { reason: "o".into() },
            })
        }
    }

    struct AlwaysContinue;

    impl LocalReasoner for AlwaysContinue {
        fn reason(&self, _i: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
            Ok(LocalReasoningResult::Continue {
                rationale: "c".into(),
            })
        }
    }

    fn with_native(reasoner: &dyn LocalReasoner) -> String {
        let mut report = run().unwrap();
        let result = evaluate(reasoner, &report.cases);
        report.native = NativeOutcome::Evaluated {
            result,
            description: "test/model (revision r), backend onnx, target Cpu".into(),
            init: std::time::Duration::from_millis(5),
            samples: vec![crate::native::Sample { confidence: 0.9 }],
        };
        render(&report)
    }

    #[test]
    fn a_safe_native_model_shows_its_improvement_over_the_baseline() {
        let text = with_native(&Oracle);
        assert!(
            text.contains("Native model:") && text.contains("False continues: 0"),
            "{text}"
        );
        assert!(text.contains("Safe improvement over Rust baseline: 4 needless escalation(s) recovered, 0 false continues"), "{text}");
        assert!(
            text.contains("Model: test/model")
                && text.contains("Mean confidence (observational only)")
        );
    }

    #[test]
    fn an_unsafe_native_model_is_not_credited_even_if_more_accurate_overall() {
        let text = with_native(&AlwaysContinue);
        assert!(
            text.contains("Safe improvement over Rust baseline: none"),
            "{text}"
        );
        assert!(!text.contains("recovered, 0 false continues"));
    }

    #[test]
    fn an_unavailable_native_model_is_skipped_not_failed() {
        let mut report = run().unwrap();
        report.native = NativeOutcome::Skipped("model unavailable".into());
        let text = render(&report);
        assert!(
            text.contains("Native model: skipped — model unavailable"),
            "{text}"
        );
        assert!(text.contains("Rust:") && text.contains("WASM:"));
    }

    #[derive(Default)]
    struct Model(AtomicUsize);

    #[async_trait::async_trait]
    impl ModelProvider for Model {
        async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(FxError::Provider("must not be called".into()))
        }
    }

    #[derive(Default)]
    struct Exec(AtomicUsize);

    #[async_trait::async_trait]
    impl Executor for Exec {
        async fn execute(&self, _r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(ExecutionError::ExecutorUnavailable(
                "must not be called".into(),
            ))
        }
    }

    struct Counted {
        calls: AtomicUsize,
        inner: TestLocalReasoner,
    }

    impl LocalReasoner for Counted {
        fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.reason(input)
        }
    }

    /// Chip handles evidence first: valid evidence is reused and the reasoner is
    /// never consulted; stale or unknown cases reach it exactly once. No model, no
    /// execution. Fixture evidence is written directly (setup only) so nothing runs.
    #[tokio::test]
    async fn valid_evidence_bypasses_the_reasoner_and_the_rest_reach_it_once() {
        for case in corpus() {
            let model = Arc::new(Model::default());
            let exec = Arc::new(Exec::default());
            let reasoner = Arc::new(Counted {
                calls: AtomicUsize::new(0),
                inner: TestLocalReasoner::default(),
            });
            let agent = Agent::new(model.clone())
                .with_executor(exec.clone())
                .with_local_reasoner(reasoner.clone());
            let request = CapabilityRequest {
                execution_id: ExecutionId::new("fixture"),
                capability_id: case.input.capability.clone(),
                inputs: case.input.inputs.clone(),
            };
            let (f1, f2) = (StateToken::new("F1"), StateToken::new("F2"));
            let fixture = Observation {
                execution_id: ExecutionId::new("fixture"),
                kind: ObservationKind::ExecutionCompleted,
                status: ExecutionStatus::Success,
                output: None,
                receipt_id: None,
            };
            let (current, expected_calls) = match case.input.evidence {
                EvidenceState::KnownValid => {
                    agent
                        .record_evidence_under(&request, &fixture, &f1)
                        .unwrap();
                    (f1.clone(), 0)
                }
                EvidenceState::KnownStale => {
                    agent
                        .record_evidence_under(&request, &fixture, &f1)
                        .unwrap();
                    (f2.clone(), 1)
                }
                EvidenceState::Unknown => (f1.clone(), 1),
            };
            let assessment = agent.assess_evidence(&request, Some(&current)).unwrap();
            assert_eq!(
                reasoner.calls.load(Ordering::SeqCst),
                expected_calls,
                "{}",
                case.id
            );
            assert_eq!(model.0.load(Ordering::SeqCst), 0, "{}", case.id);
            assert_eq!(exec.0.load(Ordering::SeqCst), 0, "{}", case.id);
            match (case.input.evidence, &assessment) {
                (EvidenceState::KnownValid, Assessment::Reuse(_)) => {
                    assert_eq!(case.expected, Verdict::Continue);
                }
                (_, Assessment::Continue { .. }) => {}
                (_, Assessment::Escalate { .. }) => {}
                (state, other) => panic!("{}: {state:?} gave {other:?}", case.id),
            }
            // The reasoner's advice did not create evidence.
            if case.input.evidence == EvidenceState::Unknown {
                assert_eq!(
                    agent.lookup_valid_evidence(&request, &current),
                    EvidenceLookup::NotFound
                );
            }
        }
    }
}
