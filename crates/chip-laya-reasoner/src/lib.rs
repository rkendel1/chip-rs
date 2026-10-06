//! Experimental `LocalReasoner` backed by the native Rust Laya typed-decision model
//! (`laya-decision`, imported as `laya`).
//!
//! Laya scores a typed question in one forward pass over a structured state; it does not
//! generate text. This adapter asks exactly one question, a two-way choice between CONTINUE
//! and ESCALATE, about the structured `ReasoningInput` and nothing else. The answer is advice:
//! anything but a well-formed CONTINUE or ESCALATE fails closed, and confidence, probabilities
//! and provenance are recorded for evaluation without ever becoming authority.
//!
//! Model acquisition is out of scope and never happens here: `from_dir` takes an explicit
//! local checkpoint directory and does not download. Inference is local Candle on CPU.
//!
//! Without the `laya` feature this crate is empty, so the default workspace build does not
//! compile candle or the tokenizer.

#![cfg(feature = "laya")]

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chip_core::{
    EvidenceState, InputValue, LocalReasoner, LocalReasoningResult, ReasoningError, ReasoningInput,
};
use indexmap::IndexMap;
use laya::LayaError;
use laya::agent::{Agent, LoadOptions, SystemOneResult};
use serde_json::{Map, Value, json};

pub use laya;

/// The only two answers accepted.
pub const CONTINUE_LABEL: &str = "CONTINUE";
pub const ESCALATE_LABEL: &str = "ESCALATE";
/// The single question asked.
pub const QUESTION_ID: &str = "verdict";
/// Version marker of the structured state layout.
pub const STATE_SCHEMA: &str = "chip.reasoning.v1";

const INSTRUCTIONS: &str = "Given the structured operational state, should the existing state be \
used as is? Choose CONTINUE only when the facts clearly support proceeding. Choose ESCALATE when \
information is missing, conflicting or ambiguous, or when a more capable reasoner is needed.";

/// The structured state Laya receives. Built only from the `ReasoningInput`, with a fixed key
/// order and sorted inputs, so the same input always yields byte-identical JSON.
pub fn encode_state(input: &ReasoningInput) -> Value {
    let evidence = match input.evidence {
        EvidenceState::KnownValid => "valid",
        EvidenceState::KnownStale => "stale",
        EvidenceState::Unknown => "unknown",
    };
    let mut inputs = Map::new();
    for (name, value) in &input.inputs {
        inputs.insert(
            name.clone(),
            match value {
                InputValue::Text(text) => Value::String(text.clone()),
                InputValue::Integer(number) => json!(number),
                InputValue::Bool(flag) => Value::Bool(*flag),
            },
        );
    }
    let mut state = Map::new();
    state.insert("schema".into(), Value::String(STATE_SCHEMA.into()));
    state.insert(
        "capability".into(),
        Value::String(input.capability.as_str().into()),
    );
    state.insert("inputs".into(), Value::Object(inputs));
    state.insert("evidence_state".into(), Value::String(evidence.into()));
    Value::Object(state)
}

/// The single typed question: a choice between CONTINUE and ESCALATE.
pub fn questions() -> IndexMap<String, Value> {
    let mut questions = IndexMap::new();
    questions.insert(
        QUESTION_ID.to_string(),
        json!({
            "type": "choice",
            "instructions": INSTRUCTIONS,
            "criteria": {
                CONTINUE_LABEL: "the existing state is sufficient; proceed",
                ESCALATE_LABEL: "a more capable reasoner must decide",
            }
        }),
    );
    questions
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayaVerdict {
    Continue,
    Escalate,
}

/// One judgment with everything Laya reported. For evaluation only: nothing here, including
/// the confidences, is authority, and no threshold is applied to any of it.
#[derive(Debug, Clone, PartialEq)]
pub struct LayaJudgment {
    pub verdict: LayaVerdict,
    pub result: LocalReasoningResult,
    /// Laya's `confidence` for a choice (derived from the entropy of the distribution).
    pub confidence: f32,
    /// Laya's calibrated `answer_confidence`.
    pub answer_confidence: f32,
    pub probabilities: BTreeMap<String, f32>,
    /// Laya's `act_probability` head.
    pub act_probability: f32,
    pub input_tokens: usize,
    /// Wall time of the forward pass as seen by the adapter.
    pub latency: Duration,
}

/// What the Laya API exposes about the model. Observational only. The API reports no
/// checkpoint revision, so none is claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayaProvenance {
    /// Model name as reported by Laya in its results, when known.
    pub model: Option<String>,
    pub path: PathBuf,
    pub backend: &'static str,
    /// Upstream Laya version the Rust port tracks.
    pub upstream_version: &'static str,
}

impl fmt::Display for LayaProvenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (path {}), backend {}, upstream Laya {}, revision not reported by the API",
            self.model.as_deref().unwrap_or("laya"),
            self.path.display(),
            self.backend,
            self.upstream_version
        )
    }
}

/// The part of the Laya engine this adapter uses, so the mapping can be tested without
/// weights. `laya::agent::Agent` is the real implementation.
pub trait TypedDecider: Send + Sync {
    fn decide(
        &self,
        state: &Value,
        questions: &IndexMap<String, Value>,
    ) -> Result<SystemOneResult, LayaError>;
}

impl TypedDecider for Agent {
    fn decide(
        &self,
        state: &Value,
        questions: &IndexMap<String, Value>,
    ) -> Result<SystemOneResult, LayaError> {
        self.system_one(state, questions)
    }
}

fn invalid(why: impl fmt::Display) -> ReasoningError {
    ReasoningError::Failed(format!("Laya output rejected: {why}"))
}

fn number(value: Option<&Value>, what: &str) -> Result<f32, ReasoningError> {
    match value.and_then(Value::as_f64) {
        Some(n) if n.is_finite() => Ok(n as f32),
        _ => Err(invalid(format!("{what} is missing or not a finite number"))),
    }
}

/// Reads Laya's answer strictly. Exactly one answer, the one asked for, a `choice` whose
/// value is exactly CONTINUE or ESCALATE, with a probability for each label that is
/// consistent with the choice. Anything else is an error.
pub fn interpret(
    result: &SystemOneResult,
    latency: Duration,
) -> Result<LayaJudgment, ReasoningError> {
    if result.answers.len() != 1 {
        return Err(invalid("expected exactly one answer"));
    }
    let answer = result
        .answers
        .get(QUESTION_ID)
        .ok_or_else(|| invalid("the verdict answer is missing"))?;
    if answer.get("type").and_then(Value::as_str) != Some("choice") {
        return Err(invalid("the answer is not a choice"));
    }
    let verdict = match answer.get("choice").and_then(Value::as_str) {
        Some(CONTINUE_LABEL) => LayaVerdict::Continue,
        Some(ESCALATE_LABEL) => LayaVerdict::Escalate,
        Some(_) => return Err(invalid("the choice is neither CONTINUE nor ESCALATE")),
        None => return Err(invalid("the choice is missing")),
    };
    let raw = answer
        .get("probabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("probabilities are missing"))?;
    if raw.len() != 2 {
        return Err(invalid(
            "expected a probability for exactly CONTINUE and ESCALATE",
        ));
    }
    let mut probabilities = BTreeMap::new();
    for label in [CONTINUE_LABEL, ESCALATE_LABEL] {
        let p = number(raw.get(label), "a probability")?;
        if !(0.0..=1.0).contains(&p) {
            return Err(invalid("a probability is outside [0, 1]"));
        }
        probabilities.insert(label.to_string(), p);
    }
    let (chosen, other) = match verdict {
        LayaVerdict::Continue => (probabilities[CONTINUE_LABEL], probabilities[ESCALATE_LABEL]),
        LayaVerdict::Escalate => (probabilities[ESCALATE_LABEL], probabilities[CONTINUE_LABEL]),
    };
    if chosen < other {
        return Err(invalid("the choice contradicts its own probabilities"));
    }
    let local = match verdict {
        LayaVerdict::Continue => LocalReasoningResult::Continue {
            rationale: "Laya typed decision".into(),
        },
        LayaVerdict::Escalate => LocalReasoningResult::Escalate {
            reason: "Laya typed decision".into(),
        },
    };
    Ok(LayaJudgment {
        verdict,
        result: local,
        confidence: number(answer.get("confidence"), "confidence")?,
        answer_confidence: number(answer.get("answer_confidence"), "answer_confidence")?,
        probabilities,
        act_probability: number(
            answer.get("action").and_then(|a| a.get("act_probability")),
            "act_probability",
        )?,
        input_tokens: result.input_tokens,
        latency,
    })
}

/// A `LocalReasoner` over a loaded Laya checkpoint. The model is loaded once and reused for
/// every judgment.
pub struct LayaReasoner {
    decider: Box<dyn TypedDecider>,
    provenance: LayaProvenance,
}

impl fmt::Debug for LayaReasoner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayaReasoner")
            .field("provenance", &self.provenance)
            .finish()
    }
}

impl LayaReasoner {
    /// Loads a checkpoint from an explicit local directory (the one holding
    /// `rl_agent_config.json` and `model.safetensors`). Never downloads.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, ReasoningError> {
        Self::load(dir.as_ref(), None)
    }

    /// Like `from_dir`, for a bundle with the checkpoint in a subfolder (for example
    /// `typed-decisions`).
    pub fn from_dir_subfolder(
        dir: impl AsRef<Path>,
        subfolder: &str,
    ) -> Result<Self, ReasoningError> {
        Self::load(dir.as_ref(), Some(subfolder.to_string()))
    }

    fn load(dir: &Path, subfolder: Option<String>) -> Result<Self, ReasoningError> {
        let unavailable = |why: String| ReasoningError::Unavailable(format!("Laya model {why}"));
        // Only an existing local directory is ever handed to Laya. Resolving it to an absolute
        // path keeps a bare name from being treated as a Hub repo id or checkpoint alias.
        let absolute = dir
            .canonicalize()
            .map_err(|_| unavailable(format!("not installed: {} does not exist", dir.display())))?;
        if !absolute.is_dir() {
            return Err(unavailable(format!(
                "not installed: {} is not a directory",
                dir.display()
            )));
        }
        let agent = Agent::load(
            &absolute.to_string_lossy(),
            LoadOptions {
                subfolder,
                device: None,
                token: None,
            },
        )
        .map_err(|error| match error {
            LayaError::NotFound(why) => unavailable(format!("not installed: {why}")),
            other => ReasoningError::Failed(format!("invalid Laya model: {other}")),
        })?;
        Ok(Self {
            decider: Box::new(agent),
            provenance: LayaProvenance {
                model: None,
                path: absolute,
                backend: "candle-cpu",
                upstream_version: laya::UPSTREAM_VERSION,
            },
        })
    }

    /// Wraps any decider (used to test the mapping without weights).
    pub fn from_decider(decider: Box<dyn TypedDecider>, provenance: LayaProvenance) -> Self {
        Self {
            decider,
            provenance,
        }
    }

    pub fn provenance(&self) -> &LayaProvenance {
        &self.provenance
    }

    /// One forward pass with the full judgment, for evaluation.
    pub fn judge(&self, input: &ReasoningInput) -> Result<LayaJudgment, ReasoningError> {
        let state = encode_state(input);
        let started = Instant::now();
        let result = self
            .decider
            .decide(&state, &questions())
            .map_err(|error| ReasoningError::Failed(format!("Laya inference failed: {error}")))?;
        interpret(&result, started.elapsed())
    }
}

impl LocalReasoner for LayaReasoner {
    fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
        self.judge(input).map(|judgment| judgment.result)
    }
}
