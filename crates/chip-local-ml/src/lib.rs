//! A `LocalReasoner` backed by a local typed-decision model, through the published
//! `rust-ml-runtime` API.
//!
//! The model is intelligence only. It is asked one constrained question, CONTINUE or
//! ESCALATE, about the structured `ReasoningInput` and nothing else. Its answer is
//! advice: any other output fails closed, confidence and provenance are recorded
//! for evaluation and never become authority, and nothing here can execute work,
//! write evidence, or reach the network (inference from an installed model is local;
//! only installation needs the network).
//!
//! Without the `runtime` feature this crate is empty, so the default workspace build
//! does not need ONNX Runtime.

#![cfg(feature = "runtime")]

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use chip_core::{
    EvidenceState, InputValue, LocalReasoner, LocalReasoningResult, ReasoningError, ReasoningInput,
};
use rust_ml_runtime::{
    DecisionModel, DecisionOption, DecisionQuestion, DecisionRequest, DecisionResult, DecisionType,
    DecisionValue, InstalledModelStatus, Runtime, RuntimeError,
};
use serde_json::{Map, Value, json};

pub use rust_ml_runtime;

/// The only two answers the model may give.
pub const CONTINUE_LABEL: &str = "CONTINUE";
pub const ESCALATE_LABEL: &str = "ESCALATE";
/// Name of the single question asked of the model.
pub const QUESTION_NAME: &str = "verdict";
/// Identifies the structured-state layout the model receives.
pub const STATE_SCHEMA: &str = "chip.local-reasoning.v1";

const INSTRUCTIONS: &str = "Given the structured operational state, answer CONTINUE to proceed \
with the existing state, or ESCALATE if a more capable reasoner is needed. Answer ESCALATE \
when information is missing, conflicting or ambiguous.";

/// Why a model could not be loaded. `Unavailable` is optional infrastructure that is
/// not installed or not usable here; `Invalid` is a model or runtime that is present but wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalMlError {
    Unavailable(String),
    Invalid(String),
}

impl fmt::Display for LocalMlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(f, "local model unavailable: {reason}"),
            Self::Invalid(reason) => write!(f, "local model invalid: {reason}"),
        }
    }
}

impl std::error::Error for LocalMlError {}

fn classify(error: RuntimeError) -> LocalMlError {
    match error {
        RuntimeError::ModelNotFound { .. }
        | RuntimeError::ModelUnavailable { .. }
        | RuntimeError::BackendUnavailable { .. }
        | RuntimeError::ProviderUnavailable { .. } => LocalMlError::Unavailable(error.to_string()),
        other => LocalMlError::Invalid(other.to_string()),
    }
}

/// What the runtime reports about the loaded model. Observational only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProvenance {
    pub model: String,
    pub revision: Option<String>,
    pub backend: String,
    /// Execution target as reported by the model's capabilities (for example `Cpu`).
    pub device: String,
    pub artifact_sha256: String,
    /// Version of the runtime crate this adapter is linked against.
    pub runtime_version: String,
}

impl fmt::Display for ModelProvenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (revision {}), backend {}, target {}, runtime {}, sha256 {}",
            self.model,
            self.revision.as_deref().unwrap_or("unknown"),
            self.backend,
            self.device,
            self.runtime_version,
            self.artifact_sha256
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelVerdict {
    Continue,
    Escalate,
}

/// One model judgment with everything the runtime reported. For evaluation only:
/// nothing in it is authority, including `confidence`.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgment {
    pub verdict: ModelVerdict,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
    pub action_probability: f64,
    /// Latency reported by the model.
    pub latency: Duration,
    pub input_tokens: u64,
    /// Runtime version stamped on this result by the runtime itself.
    pub result_runtime_version: String,
}

/// The structured input the model receives, built only from the `ReasoningInput`.
/// The same input always produces the same value.
pub fn encode_state(input: &ReasoningInput) -> Value {
    let evidence = match input.evidence {
        EvidenceState::KnownValid => "known_valid",
        EvidenceState::KnownStale => "known_stale",
        EvidenceState::Unknown => "unknown",
    };
    let mut inputs = Map::new();
    for (name, value) in &input.inputs {
        let encoded = match value {
            InputValue::Text(text) => json!({ "type": "text", "value": text }),
            InputValue::Integer(number) => json!({ "type": "integer", "value": number }),
            InputValue::Bool(flag) => json!({ "type": "bool", "value": flag }),
        };
        inputs.insert(name.clone(), encoded);
    }
    json!({
        "schema": STATE_SCHEMA,
        "capability": input.capability.as_str(),
        "evidence": evidence,
        "inputs": Value::Object(inputs),
    })
}

/// The decision request for one reasoning input: one two-way choice.
pub fn build_request(input: &ReasoningInput) -> DecisionRequest {
    DecisionRequest {
        state: encode_state(input),
        decisions: vec![DecisionQuestion {
            name: QUESTION_NAME.to_owned(),
            instructions: INSTRUCTIONS.to_owned(),
            kind: DecisionType::Choice {
                options: vec![
                    DecisionOption {
                        label: CONTINUE_LABEL.to_owned(),
                        description: Some(Value::String(
                            "The existing state is sufficient; proceed.".to_owned(),
                        )),
                    },
                    DecisionOption {
                        label: ESCALATE_LABEL.to_owned(),
                        description: Some(Value::String(
                            "A more capable reasoner must decide.".to_owned(),
                        )),
                    },
                ],
            },
        }],
    }
}

/// Reads the model's result strictly. Exactly one decision, named as asked, with a
/// choice value that is exactly one of the two labels; anything else is an error.
pub fn interpret(result: &DecisionResult) -> Result<Judgment, ReasoningError> {
    let bad = |why: &str| ReasoningError::Failed(format!("local model output rejected: {why}"));
    let [decision] = result.decisions.as_slice() else {
        return Err(bad("expected exactly one decision"));
    };
    if decision.name != QUESTION_NAME {
        return Err(bad("decision has an unexpected name"));
    }
    let verdict = match &decision.value {
        DecisionValue::Choice(label) if label == CONTINUE_LABEL => ModelVerdict::Continue,
        DecisionValue::Choice(label) if label == ESCALATE_LABEL => ModelVerdict::Escalate,
        DecisionValue::Choice(_) => return Err(bad("choice is neither CONTINUE nor ESCALATE")),
        _ => return Err(bad("decision is not a choice")),
    };
    Ok(Judgment {
        verdict,
        confidence: decision.confidence,
        probabilities: decision.probabilities.clone(),
        action_probability: decision.action_probability,
        latency: result.execution.latency,
        input_tokens: result.execution.input_tokens,
        result_runtime_version: result.provenance.runtime_version.clone(),
    })
}

/// A `LocalReasoner` over a loaded local decision model.
pub struct RustMLReasoner {
    model: Box<dyn DecisionModel>,
    provenance: ModelProvenance,
}

impl fmt::Debug for RustMLReasoner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RustMLReasoner")
            .field("provenance", &self.provenance)
            .finish()
    }
}

impl RustMLReasoner {
    /// Wraps an already loaded model.
    pub fn from_model(model: Box<dyn DecisionModel>) -> Self {
        let description = model.describe();
        let capabilities = model.capabilities();
        let provenance = ModelProvenance {
            model: description.identifier,
            revision: description.revision,
            backend: description.backend,
            device: format!("{:?}", capabilities.device),
            artifact_sha256: description.artifact_sha256,
            runtime_version: rust_ml_runtime::VERSION.to_owned(),
        };
        Self { model, provenance }
    }

    /// Loads a registered, installed model (for example `"laya"`) through the runtime.
    pub fn load_installed(runtime: &Runtime, name: &str) -> Result<Self, LocalMlError> {
        runtime
            .load_model(name)
            .map(Self::from_model)
            .map_err(classify)
    }

    /// Loads a typed-decision artifact directory through the single matching provider.
    pub fn load_artifact(
        runtime: &Runtime,
        artifact: impl AsRef<std::path::Path>,
    ) -> Result<Self, LocalMlError> {
        runtime
            .load_decision_model(artifact)
            .map(Self::from_model)
            .map_err(classify)
    }

    pub fn provenance(&self) -> &ModelProvenance {
        &self.provenance
    }

    /// One model call with the full judgment, for evaluation.
    pub fn judge(&self, input: &ReasoningInput) -> Result<Judgment, ReasoningError> {
        let result = self
            .model
            .decide(&build_request(input))
            .map_err(|error| ReasoningError::Failed(format!("local model failed: {error}")))?;
        interpret(&result)
    }
}

impl LocalReasoner for RustMLReasoner {
    fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
        Ok(match self.judge(input)?.verdict {
            ModelVerdict::Continue => LocalReasoningResult::Continue {
                rationale: "local model verdict".to_owned(),
            },
            ModelVerdict::Escalate => LocalReasoningResult::Escalate {
                reason: "local model verdict".to_owned(),
            },
        })
    }
}

/// Whether `name` is installed and ready for local inference, with the status text.
pub fn installation_status(
    runtime: &Runtime,
    name: &str,
) -> Result<InstalledModelStatus, LocalMlError> {
    runtime
        .installed_models()
        .map_err(classify)?
        .into_iter()
        .find(|record| record.model == name)
        .map(|record| record.status)
        .ok_or_else(|| {
            LocalMlError::Unavailable(format!("{name} is not registered for this platform"))
        })
}
