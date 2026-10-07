//! The decision boundary: model output is data; Chip decides whether that data
//! is a capability request. Nothing here can perform work, and nothing here
//! knows about any concrete system that does.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use fx_core::ModelResponse;

use crate::{Capability, CapabilityError, CapabilityId, ExecutionId};

/// A typed input value. Deliberately small; not an expression language.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum InputValue {
    Text(String),
    Integer(i64),
    Bool(bool),
}

/// A semantic request for a declared capability. It names *what* is wanted,
/// never *how* it is run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRequest {
    pub execution_id: ExecutionId,
    pub capability_id: CapabilityId,
    pub inputs: BTreeMap<String, InputValue>,
    /// Whether the requester sent an `inputs` member at all, even an empty one. A capability that
    /// declares no inputs accepts no such member.
    pub inputs_present: bool,
}

impl CapabilityRequest {
    pub fn new(execution_id: ExecutionId, capability_id: CapabilityId) -> Self {
        Self {
            execution_id,
            capability_id,
            inputs: BTreeMap::new(),
            inputs_present: false,
        }
    }

    pub fn with_input(mut self, name: impl Into<String>, value: InputValue) -> Self {
        self.inputs.insert(name.into(), value);
        self.inputs_present = true;
        self
    }
}

/// What the agent decided. There is intentionally no variant that performs
/// work: that belongs to a separate boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentDecision {
    Respond(ModelResponse),
    RequestCapability(CapabilityRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionError {
    InvalidDecision(String),
    Capability(CapabilityError),
}

impl fmt::Display for DecisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDecision(message) => write!(f, "invalid decision: {message}"),
            Self::Capability(error) => write!(f, "decision rejected: {error}"),
        }
    }
}

impl Error for DecisionError {}

impl From<CapabilityError> for DecisionError {
    fn from(error: CapabilityError) -> Self {
        Self::Capability(error)
    }
}

/// Converts model output into a semantic decision. Pure and synchronous: it can
/// only read what it is given, and it can only return data.
pub trait DecisionBoundary: Send + Sync {
    fn decide(
        &self,
        response: &ModelResponse,
        capabilities: &[Capability],
    ) -> Result<AgentDecision, DecisionError>;
}

/// Explicitly structured input for the deterministic boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionInput {
    Respond,
    RequestCapability {
        execution_id: ExecutionId,
        /// Raw id; it is validated when the decision is made.
        capability_id: String,
        inputs: BTreeMap<String, InputValue>,
    },
}

/// Test-oriented boundary that returns a pre-set decision. It never reads the
/// text of the model response, so model text cannot become a request.
#[derive(Debug, Clone)]
pub struct ScriptedDecision {
    input: DecisionInput,
}

impl ScriptedDecision {
    pub fn new(input: DecisionInput) -> Self {
        Self { input }
    }
}

impl DecisionBoundary for ScriptedDecision {
    fn decide(
        &self,
        response: &ModelResponse,
        _capabilities: &[Capability],
    ) -> Result<AgentDecision, DecisionError> {
        match &self.input {
            DecisionInput::Respond => Ok(AgentDecision::Respond(response.clone())),
            DecisionInput::RequestCapability {
                execution_id,
                capability_id,
                inputs,
            } => Ok(AgentDecision::RequestCapability(CapabilityRequest {
                execution_id: execution_id.clone(),
                capability_id: CapabilityId::new(capability_id.clone())?,
                inputs_present: !inputs.is_empty(),
                inputs: inputs.clone(),
            })),
        }
    }
}
