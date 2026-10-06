//! The optional native local-model reasoner (Laya via rust-ml-runtime).
//!
//! Without the `local-ml` feature, or without an installed and READY model, `load`
//! returns the reason it is skipped. Nothing here falls back to another reasoner and
//! unavailable infrastructure is never reported as a model failure.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chip_core::LocalReasoner;

/// One recorded model judgment, for evaluation output only.
#[derive(Debug, Clone)]
pub struct Sample {
    pub confidence: f64,
}

pub struct Native {
    pub reasoner: Arc<dyn LocalReasoner>,
    /// Model identity, version, backend, target and runtime.
    pub description: String,
    /// Runtime construction plus model load. Reported apart from inference.
    pub init: Duration,
    /// Judgments in call order since the last `take_samples`.
    pub log: Arc<Mutex<Vec<Sample>>>,
}

impl Native {
    pub fn take_samples(&self) -> Vec<Sample> {
        std::mem::take(&mut *self.log.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[cfg(not(feature = "local-ml"))]
pub fn load() -> Result<Native, String> {
    Err("built without the local-ml feature".to_string())
}

#[cfg(feature = "local-ml")]
pub fn load() -> Result<Native, String> {
    use chip_local_ml::rust_ml_runtime::{InstalledModelStatus, Runtime};
    use chip_local_ml::{RustMLReasoner, installation_status};

    let started = std::time::Instant::now();
    // Remote fallback is never allowed: inference from an installed model is local.
    let runtime = Runtime::builder().allow_remote_fallback(false).build();
    match installation_status(&runtime, "laya").map_err(|e| e.to_string())? {
        InstalledModelStatus::Ready => {}
        other => {
            return Err(format!(
                "model unavailable: laya is {other}; install it with `ml-runtime model install laya`"
            ));
        }
    }
    let reasoner = RustMLReasoner::load_installed(&runtime, "laya").map_err(|e| e.to_string())?;
    let init = started.elapsed();
    let description = reasoner.provenance().to_string();
    let log = Arc::new(Mutex::new(Vec::new()));
    Ok(Native {
        reasoner: Arc::new(recording::Recording {
            inner: reasoner,
            log: log.clone(),
        }),
        description,
        init,
        log,
    })
}

#[cfg(feature = "local-ml")]
mod recording {
    use std::sync::{Arc, Mutex};

    use chip_core::{LocalReasoner, LocalReasoningResult, ReasoningError, ReasoningInput};
    use chip_local_ml::{ModelVerdict, RustMLReasoner};

    use super::Sample;

    /// Records each judgment's verdict and confidence, then returns the same result
    /// the adapter would. Confidence is kept for output only.
    pub struct Recording {
        pub inner: RustMLReasoner,
        pub log: Arc<Mutex<Vec<Sample>>>,
    }

    impl LocalReasoner for Recording {
        fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
            let judgment = self.inner.judge(input)?;
            let result = match judgment.verdict {
                ModelVerdict::Continue => LocalReasoningResult::Continue {
                    rationale: "local model verdict".into(),
                },
                ModelVerdict::Escalate => LocalReasoningResult::Escalate {
                    reason: "local model verdict".into(),
                },
            };
            self.log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Sample {
                    confidence: judgment.confidence,
                });
            Ok(result)
        }
    }
}

/// `--test-local-model-reasoner`: the evidence hierarchy with the real local model.
#[cfg(feature = "local-ml")]
pub fn demo(native: &Native) -> Result<String, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chip_core::{
        Agent, Assessment, CapabilityId, CapabilityRequest, ExecutionId, ExecutionStatus,
        Observation, ObservationKind, StateToken,
    };
    use chip_local_ml::rust_ml_runtime::{
        DecisionExecution, DecisionProvenance, DecisionResult, DecisionValue, ModelIdentity,
        TypedDecision,
    };
    use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse};

    struct RemoteModel(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl ModelProvider for RemoteModel {
        async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(FxError::Provider(
                "the remote model must not be called".into(),
            ))
        }
    }

    let remote = Arc::new(AtomicUsize::new(0));
    // No executor is injected: the agent cannot execute anything.
    let agent = Agent::new(Arc::new(RemoteModel(remote.clone())))
        .with_local_reasoner(native.reasoner.clone());
    let (f1, f2) = (StateToken::new("F1"), StateToken::new("F2"));
    let request = |id: &str, capability: &str| {
        CapabilityId::new(capability)
            .map(|c| CapabilityRequest::new(ExecutionId::new(id), c))
            .map_err(|e| e.to_string())
    };
    let mut out = String::from("Native Local Model Reasoner\n\n");
    out += &format!(
        "Model: {}\nInitialization (runtime + model load): {}\n\n",
        native.description,
        crate::benchmark::fmt(native.init)
    );

    // 1. Valid evidence is reused; the model is not consulted. (Fixture evidence, written directly.)
    let known = request("fixture", "tests.run")?;
    let fixture = Observation {
        execution_id: ExecutionId::new("fixture"),
        kind: ObservationKind::ExecutionCompleted,
        status: ExecutionStatus::Success,
        output: None,
        receipt_id: None,
    };
    agent
        .record_evidence_under(&known, &fixture, &f1)
        .map_err(|e| e.to_string())?;
    native.take_samples();
    match agent
        .assess_evidence(&request("e1", "tests.run")?, Some(&f1))
        .map_err(|e| e.to_string())?
    {
        Assessment::Reuse(_) => {
            out += &format!(
                "KnownValid: evidence reused, local model calls: {}\n",
                native.take_samples().len()
            )
        }
        other => return Err(format!("valid evidence was not reused: {other:?}")),
    }

    // 2. Stale and 3. unknown evidence reach the model.
    for (label, id, capability, state) in [
        ("KnownStale", "e2", "tests.run", &f2),
        ("Unknown", "e3", "build.run", &f1),
    ] {
        let assessment = agent
            .assess_evidence(&request(id, capability)?, Some(state))
            .map_err(|e| e.to_string())?;
        let samples = native.take_samples();
        let verdict = match assessment {
            Assessment::Continue { .. } => "continue",
            Assessment::Escalate { .. } => "escalate",
            Assessment::Reuse(_) => return Err(format!("{label} unexpectedly reused evidence")),
        };
        let confidence = samples
            .first()
            .map(|s| format!("{:.3}", s.confidence))
            .unwrap_or_default();
        out += &format!(
            "{label}: local model called {} time(s), verdict {verdict} (confidence {confidence}, recorded only)\n",
            samples.len()
        );
    }

    // 4. Invalid model output fails closed. A synthetic malformed result, not a real reply.
    let malformed = DecisionResult {
        model: ModelIdentity {
            identifier: "synthetic".into(),
            revision: None,
        },
        backend: "synthetic".into(),
        decisions: vec![TypedDecision {
            name: chip_local_ml::QUESTION_NAME.into(),
            kind: "choice".into(),
            value: DecisionValue::Choice("CONTINUE, probably".into()),
            probabilities: Default::default(),
            confidence: 1.0,
            action_probability: 1.0,
        }],
        execution: DecisionExecution {
            latency: Duration::ZERO,
            input_tokens: 0,
            output_tokens: 0,
        },
        provenance: DecisionProvenance {
            artifact_path: Default::default(),
            artifact_sha256: String::new(),
            runtime_version: String::new(),
        },
    };
    match chip_local_ml::interpret(&malformed) {
        Err(error) => {
            out += &format!("Invalid output (synthetic): rejected, failed closed ({error})\n")
        }
        Ok(_) => return Err("malformed output was accepted".into()),
    }

    out += &format!(
        "\nExecutions: 0\nRemote model calls: {}\nEvidence writes by the model: 0\n",
        remote.load(Ordering::SeqCst)
    );
    if remote.load(Ordering::SeqCst) != 0 {
        return Err("the remote model was called".into());
    }
    Ok(out)
}

#[cfg(all(test, feature = "local-ml"))]
mod tests {
    use chip_core::{LocalReasoningResult, ReasoningError, ReasoningInput};

    use super::*;

    struct Stub(Arc<Mutex<Vec<Sample>>>);

    impl LocalReasoner for Stub {
        fn reason(&self, _i: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
            self.0.lock().unwrap().push(Sample { confidence: 0.75 });
            Ok(LocalReasoningResult::Escalate {
                reason: "stub".into(),
            })
        }
    }

    #[test]
    fn the_demo_bypasses_the_model_for_valid_evidence_and_calls_it_for_the_rest() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let native = Native {
            reasoner: Arc::new(Stub(log.clone())),
            description: "stub/model (revision r), backend onnx, target Cpu".into(),
            init: Duration::from_millis(1),
            log,
        };
        let text = demo(&native).unwrap();
        assert!(
            text.contains("KnownValid: evidence reused, local model calls: 0"),
            "{text}"
        );
        assert!(
            text.contains("KnownStale: local model called 1 time(s)"),
            "{text}"
        );
        assert!(
            text.contains("Unknown: local model called 1 time(s)"),
            "{text}"
        );
        assert!(
            text.contains("Invalid output (synthetic): rejected, failed closed"),
            "{text}"
        );
        assert!(text.contains("Executions: 0") && text.contains("Remote model calls: 0"));
    }
}
