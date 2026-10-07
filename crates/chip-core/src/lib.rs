mod capability_set;
mod decision;
mod decision_state;
mod evidence;
mod model_decision;
mod observation;
mod reasoning;
mod work;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex};

pub use capability_set::{CapabilityBackend, CapabilitySet};
pub use decision::{
    AgentDecision, CapabilityRequest, DecisionBoundary, DecisionError, DecisionInput, InputValue,
    ScriptedDecision,
};
pub use decision_state::{
    CapabilityDecisionState, DECISION_SCHEMA, DecisionStateError, GraphStateToken, ImpactState,
};
pub use evidence::{EvidenceError, EvidenceKey, EvidenceLookup, EvidenceStats, StateToken};
use fx_core::{Message, MessageRole, ModelProvider, ModelRequest, ModelResponse};
pub use model_decision::{ModelDecisionBoundary, WORK_DECISION_SCHEMA};
pub use observation::{
    ExecutionObserver, Observation, ObservationError, ObservationKind, Observer,
};
pub use reasoning::{
    EvidenceState, LocalReasoner, LocalReasoningResult, ReasoningError, ReasoningInput,
    TestLocalReasoner,
};
pub use work::{
    ContextCall, ContextMetrics, ContextReport, DecisionRecord, DecisionSource,
    DeduplicatedEscalationContext, EscalationContext, EscalationContextPolicy,
    FullEscalationContext, LimitKind, LocalWorkPolicy, ModelUsage, NoLocalPolicy, ObservationClass,
    ObservationInvariant, ObservationOrigin, ObservationPredicate, ObservationRepetition,
    RespondCompletes, SafetyAudit, ScriptedPolicy, TerminalState, TurnTrace, WorkDecision,
    WorkDecisionBoundary, WorkEvent, WorkGoal, WorkId, WorkLatency, WorkLimits, WorkMeasurement,
    WorkOutcome, WorkReport, WorkSpec, WorkState, WorkSummary, WorkTrajectory,
    WorkUtilityMeasurement, WorkView, audit_safety, classify_observations, context_report,
    measure_utility, omissions, trace, verify_trajectory,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub user_message: String,
}

impl Turn {
    pub fn new(user_message: impl Into<String>) -> Self {
        Self {
            user_message: user_message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    TurnReceived {
        message: String,
    },
    RequestBuilt {
        model: String,
    },
    ModelInvoked {
        model: String,
    },
    ModelResponded {
        output: String,
    },
    TurnCompleted {
        response: String,
    },
    Execution(ExecutionEvent),
    Capability(CapabilityEvent),
    TurnStarted {
        message: String,
    },
    /// `capability` is `None` for a plain response.
    DecisionMade {
        capability: Option<CapabilityId>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnResult {
    pub response: String,
    pub events: Vec<AgentEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    InvalidTurn(String),
    Provider(String),
    Decision(DecisionError),
    Capability(CapabilityError),
    Execution(ExecutionError),
    Observation(ObservationError),
    Evidence(EvidenceError),
    Reasoning(ReasoningError),
}

/// Stable, opaque name of a declared ability (for example `vendor.operation`).
/// It is a semantic name, never a command: only lowercase letters, digits,
/// `.`, `_` and `-` are accepted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CapabilityId(String);

impl CapabilityId {
    pub fn new(id: impl Into<String>) -> Result<Self, CapabilityError> {
        let id = id.into();
        let valid = !id.is_empty()
            && id.len() <= 128
            && id.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
            });
        if valid {
            Ok(Self(id))
        } else {
            Err(CapabilityError::InvalidId(id))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One named input a capability accepts. A deliberately small input contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityInput {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// What a capability is, with no implementation details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityDescriptor {
    pub id: CapabilityId,
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub inputs: Vec<CapabilityInput>,
    /// The largest a text input of this capability may be, in bytes. `None` is the boundary's
    /// default (small). Declared by the capability, never by a requester.
    pub max_input_bytes: Option<usize>,
    /// Whether an earlier observation of this capability may answer a later identical request.
    /// A capability whose result depends on state that changes between requests (a project's
    /// files, for one) is always performed again: reality is asked, not remembered.
    pub reuse_evidence: bool,
}

impl CapabilityDescriptor {
    pub fn new(id: CapabilityId, name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            description: description.into(),
            version: None,
            inputs: Vec::new(),
            max_input_bytes: None,
            reuse_evidence: true,
        }
    }

    pub fn with_max_input_bytes(mut self, bytes: usize) -> Self {
        self.max_input_bytes = Some(bytes);
        self
    }

    /// Marks the capability's observations as never reusable for a later request.
    pub fn without_evidence_reuse(mut self) -> Self {
        self.reuse_evidence = false;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityAvailability {
    Available,
    Unavailable(String),
    Misconfigured(String),
}

/// A declared ability together with whether it can currently be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub descriptor: CapabilityDescriptor,
    pub availability: CapabilityAvailability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityError {
    InvalidId(String),
    Unknown(String),
    Unavailable(String),
    InvalidInput(String),
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId(id) => write!(f, "invalid capability id: {id:?}"),
            Self::Unknown(id) => write!(f, "unknown capability: {id}"),
            Self::Unavailable(message) => write!(f, "capabilities unavailable: {message}"),
            Self::InvalidInput(message) => write!(f, "invalid capability input: {message}"),
        }
    }
}

impl Error for CapabilityError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityEvent {
    CapabilitiesRequested,
    CapabilitiesAvailable { count: usize },
    CapabilitiesUnavailable { reason: String },
}

/// Describes what is available. Discovery only: it never performs work, and
/// execution stays with `Executor`.
#[async_trait::async_trait]
pub trait CapabilityProvider: Send + Sync {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError>;

    /// Whether a described capability can currently be used. Must not perform it.
    async fn availability(&self, id: &CapabilityId) -> CapabilityAvailability;

    /// Checks the values of a request's inputs against the capability's own rules (a path that
    /// stays inside a root, a size limit), after the declared names and requiredness have been
    /// checked. The capability owns what is acceptable; Chip runs this before anything executes,
    /// so a rejected request never reaches an executor. Must not perform the capability.
    async fn validate_inputs(
        &self,
        _id: &CapabilityId,
        _inputs: &BTreeMap<String, InputValue>,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityReport {
    pub result: Result<Vec<Capability>, CapabilityError>,
    pub events: Vec<CapabilityEvent>,
}

impl ExecutionRequest {
    /// The request that asks an executor to perform a declared capability.
    pub fn for_capability(id: ExecutionId, capability: &CapabilityId) -> Self {
        Self::new(id, capability.as_str())
    }
}

/// Identifier correlating an execution request, its events and its result.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExecutionId(pub String);

impl ExecutionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A semantic request to perform work. `intent` names the operation; it is
/// not a command line and implies nothing about how it is carried out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionRequest {
    pub id: ExecutionId,
    pub intent: String,
    /// The typed inputs the capability declared and the request supplied, already validated
    /// against the declaration. Empty for a capability that declares none.
    pub inputs: BTreeMap<String, InputValue>,
}

impl ExecutionRequest {
    pub fn new(id: ExecutionId, intent: impl Into<String>) -> Self {
        Self {
            id,
            intent: intent.into(),
            inputs: BTreeMap::new(),
        }
    }

    pub fn with_inputs(mut self, inputs: BTreeMap<String, InputValue>) -> Self {
        self.inputs = inputs;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStatus {
    Success,
    Failure,
    /// The executor reports the work was cancelled.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionResult {
    pub id: ExecutionId,
    pub status: ExecutionStatus,
    pub output: String,
    /// Stable identifier of the executor's evidence for this execution, if it
    /// provides one. Chip preserves it and does not interpret it.
    pub receipt_id: Option<String>,
}

impl ExecutionResult {
    pub fn success(id: ExecutionId, output: impl Into<String>) -> Self {
        Self {
            id,
            status: ExecutionStatus::Success,
            output: output.into(),
            receipt_id: None,
        }
    }

    pub fn failure(id: ExecutionId, output: impl Into<String>) -> Self {
        Self {
            id,
            status: ExecutionStatus::Failure,
            output: output.into(),
            receipt_id: None,
        }
    }

    pub fn with_receipt_id(mut self, receipt_id: impl Into<String>) -> Self {
        self.receipt_id = Some(receipt_id.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionError {
    InvalidRequest(String),
    ExecutorUnavailable(String),
    ExecutionFailed(String),
    Cancelled,
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(f, "invalid execution request: {message}"),
            Self::ExecutorUnavailable(message) => write!(f, "executor unavailable: {message}"),
            Self::ExecutionFailed(message) => write!(f, "execution failed: {message}"),
            Self::Cancelled => write!(f, "execution cancelled"),
        }
    }
}

impl Error for ExecutionError {}

/// Semantic execution events: identifiers and descriptions only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionEvent {
    ExecutionRequested { id: ExecutionId, intent: String },
    ExecutionStarted { id: ExecutionId },
    ExecutionCompleted { id: ExecutionId, output: String },
    ExecutionFailed { id: ExecutionId, reason: String },
}

/// Performs work on behalf of Chip. Implementations are injected; dropping the
/// returned future stops waiting for the result.
#[async_trait::async_trait]
pub trait Executor: Send + Sync {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError>;
}

/// Deterministic executor for tests and demos. Performs no real work.
#[derive(Debug, Default, Clone, Copy)]
pub struct TestExecutor;

#[async_trait::async_trait]
impl Executor for TestExecutor {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        Ok(ExecutionResult::success(
            request.id,
            "test execution completed",
        ))
    }
}

/// Outcome of one execution request, with the events it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionReport {
    pub result: Result<ExecutionResult, ExecutionError>,
    pub events: Vec<ExecutionEvent>,
}

/// The outcome of `Agent::assess_evidence`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    /// Valid evidence exists; the reasoner was not consulted.
    Reuse(Observation),
    /// The local reasoner is confident the caller may proceed.
    Continue { rationale: String },
    /// The local reasoner is not confident; escalating is the caller's call.
    Escalate { reason: String },
}

/// How `Agent::obtain_evidence` satisfied a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceOutcome {
    /// Existing evidence returned unchanged; nothing was executed.
    Reused(Observation),
    /// The operation was executed once and its observation recorded.
    Performed {
        result: ExecutionResult,
        observation: Observation,
    },
}

struct DecideStep {
    turn: TurnResult,
    response: ModelResponse,
    decision: Result<AgentDecision, DecisionError>,
    capability_events: Vec<CapabilityEvent>,
}

/// In-memory result of one complete `Agent::run_turn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    pub turn: Turn,
    pub response: ModelResponse,
    pub decision: AgentDecision,
    /// `None` for a response; `Some` when a capability was executed.
    pub execution: Option<ExecutionResult>,
    pub events: Vec<AgentEvent>,
}

/// The outcome of `Agent::decide`: the model turn and the decision about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionReport {
    pub turn: TurnResult,
    pub decision: Result<AgentDecision, DecisionError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResult {
    pub turn: TurnResult,
    pub execution: ExecutionReport,
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTurn(message) => write!(f, "invalid turn: {message}"),
            Self::Provider(message) => write!(f, "provider error: {message}"),
            Self::Decision(error) => write!(f, "{error}"),
            Self::Capability(error) => write!(f, "{error}"),
            Self::Execution(error) => write!(f, "{error}"),
            Self::Observation(error) => write!(f, "{error}"),
            Self::Evidence(error) => write!(f, "{error}"),
            Self::Reasoning(error) => write!(f, "{error}"),
        }
    }
}

impl Error for AgentError {}

const DEFAULT_MODEL: &str = "chip-test-model";

pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    model: String,
    executor: Option<Arc<dyn Executor>>,
    capabilities: Option<Arc<dyn CapabilityProvider>>,
    decision: Option<Arc<dyn DecisionBoundary>>,
    observer: Option<Arc<dyn Observer>>,
    evidence: Mutex<evidence::EvidenceStore>,
    reasoner: Option<Arc<dyn LocalReasoner>>,
    max_output_tokens: Option<u32>,
}

impl Agent {
    pub fn new(provider: Arc<dyn ModelProvider>) -> Self {
        Self::with_model(provider, DEFAULT_MODEL)
    }

    pub fn with_model(provider: Arc<dyn ModelProvider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            executor: None,
            capabilities: None,
            decision: None,
            observer: None,
            evidence: Mutex::new(evidence::EvidenceStore::default()),
            reasoner: None,
            max_output_tokens: None,
        }
    }

    /// How many tokens a model may spend on one reply. A capability that takes a whole file as
    /// input needs room for it; the default is the model boundary's small one. A budget, not an
    /// authority: it changes how much a reply may say, never what it is allowed to do.
    pub fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }

    /// Optionally injects a local reasoner for cheap judgments.
    pub fn with_local_reasoner(mut self, reasoner: Arc<dyn LocalReasoner>) -> Self {
        self.reasoner = Some(reasoner);
        self
    }

    /// Evidence first, local reasoning second. Valid evidence is returned without
    /// consulting the reasoner. Otherwise the reasoner is given the evidence
    /// state (stale or unknown) and returns a verdict. This only advises: it
    /// does not execute, call the model, or record evidence, and `Escalate` leaves
    /// the explicit call to the model with the caller.
    pub fn assess_evidence(
        &self,
        request: &CapabilityRequest,
        state: Option<&StateToken>,
    ) -> Result<Assessment, AgentError> {
        let lookup = match state {
            Some(state) => self.lookup_valid_evidence(request, state),
            None => self.lookup_evidence(request),
        };
        let evidence = match lookup {
            EvidenceLookup::Found(observation) => return Ok(Assessment::Reuse(observation)),
            EvidenceLookup::Stale => EvidenceState::KnownStale,
            EvidenceLookup::NotFound => EvidenceState::Unknown,
        };
        let reasoner = self.reasoner.as_ref().ok_or_else(|| {
            AgentError::Reasoning(ReasoningError::Unavailable(
                "no local reasoner injected".to_string(),
            ))
        })?;
        let verdict = reasoner
            .reason(&ReasoningInput {
                capability: request.capability_id.clone(),
                inputs: request.inputs.clone(),
                evidence,
            })
            .map_err(AgentError::Reasoning)?;
        Ok(match verdict {
            LocalReasoningResult::Continue { rationale } => Assessment::Continue { rationale },
            LocalReasoningResult::Escalate { reason } => Assessment::Escalate { reason },
        })
    }

    fn evidence_store(&self) -> std::sync::MutexGuard<'_, evidence::EvidenceStore> {
        self.evidence.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Retains the observation of `request`'s execution as evidence that carries
    /// no state token. Only an observation of the execution this request
    /// produced is accepted, so a request alone can never become evidence.
    /// Cancelled executions did not establish an outcome and are not retained
    /// (returns `false`).
    pub fn record_evidence(
        &self,
        request: &CapabilityRequest,
        observation: &Observation,
    ) -> Result<bool, EvidenceError> {
        self.record_with_state(request, observation, None)
    }

    /// Like `record_evidence`, remembering the state it was established under.
    pub fn record_evidence_under(
        &self,
        request: &CapabilityRequest,
        observation: &Observation,
        state: &StateToken,
    ) -> Result<bool, EvidenceError> {
        self.record_with_state(request, observation, Some(state.clone()))
    }

    fn record_with_state(
        &self,
        request: &CapabilityRequest,
        observation: &Observation,
        state: Option<StateToken>,
    ) -> Result<bool, EvidenceError> {
        if observation.execution_id != request.execution_id {
            return Err(EvidenceError::ExecutionMismatch(format!(
                "observation is of execution {}, not {}",
                observation.execution_id, request.execution_id
            )));
        }
        if observation.kind == ObservationKind::ExecutionCancelled {
            return Ok(false);
        }
        self.evidence_store().record(
            EvidenceKey::from_request(request),
            observation.clone(),
            state,
        );
        Ok(true)
    }

    /// Deterministic lookup of evidence that carries no state token. Evidence
    /// recorded under a state is `Stale` here: without a current state it cannot
    /// be shown to be current. No model, no execution.
    pub fn lookup_evidence(&self, request: &CapabilityRequest) -> EvidenceLookup {
        self.evidence_store()
            .lookup(&EvidenceKey::from_request(request), None)
    }

    /// Lookup against the caller-supplied current state. Evidence established
    /// under a different state is `Stale`, never returned as current.
    pub fn lookup_valid_evidence(
        &self,
        request: &CapabilityRequest,
        current: &StateToken,
    ) -> EvidenceLookup {
        self.evidence_store()
            .lookup(&EvidenceKey::from_request(request), Some(current))
    }

    /// Explicitly discards all evidence for a capability. Returns how many
    /// entries were removed.
    pub fn invalidate_evidence(&self, capability: &CapabilityId) -> usize {
        self.evidence_store().invalidate(capability)
    }

    pub fn evidence_stats(&self) -> EvidenceStats {
        self.evidence_store().stats()
    }

    /// The fast path: reuse existing stateless evidence for this operation,
    /// otherwise validate, execute once, observe, and record.
    pub async fn obtain_evidence(
        &self,
        request: &CapabilityRequest,
    ) -> Result<EvidenceOutcome, AgentError> {
        self.obtain_with_state(request, None).await
    }

    /// The fast path under an explicit current state. Evidence established under
    /// the same state is reused with no model call and no execution; missing or
    /// stale evidence causes one execution whose observation replaces it.
    pub async fn obtain_evidence_under(
        &self,
        request: &CapabilityRequest,
        state: &StateToken,
    ) -> Result<EvidenceOutcome, AgentError> {
        self.obtain_with_state(request, Some(state)).await
    }

    async fn obtain_with_state(
        &self,
        request: &CapabilityRequest,
        state: Option<&StateToken>,
    ) -> Result<EvidenceOutcome, AgentError> {
        let lookup = match state {
            Some(state) => self.lookup_valid_evidence(request, state),
            None => self.lookup_evidence(request),
        };
        if let EvidenceLookup::Found(observation) = lookup {
            return Ok(EvidenceOutcome::Reused(observation));
        }
        let execution = self
            .validate_capability_request(request)
            .await
            .map_err(AgentError::Capability)?;
        let report = self.execute(execution).await;
        let result = report.result.map_err(AgentError::Execution)?;
        let observation = self.observe(&result).map_err(AgentError::Observation)?;
        self.record_with_state(request, &observation, state.cloned())
            .map_err(AgentError::Evidence)?;
        Ok(EvidenceOutcome::Performed {
            result,
            observation,
        })
    }

    /// Optionally injects the observer that represents execution results.
    pub fn with_observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Represents an execution result as an `Observation` via the injected
    /// observer. Does not execute, call the model, or start another turn.
    pub fn observe(&self, result: &ExecutionResult) -> Result<Observation, ObservationError> {
        match &self.observer {
            Some(observer) => observer.observe(result),
            None => Err(ObservationError::ObserverUnavailable(
                "no observer injected".to_string(),
            )),
        }
    }

    /// Optionally injects the boundary that turns model output into a decision.
    pub fn with_decision_boundary(mut self, boundary: Arc<dyn DecisionBoundary>) -> Self {
        self.decision = Some(boundary);
        self
    }

    /// One model turn, then one decision about its response. Decides only;
    /// nothing is validated against the executor or executed.
    pub async fn decide(&self, turn: Turn) -> Result<DecisionReport, AgentError> {
        self.decide_with_observations(turn, &[]).await
    }

    /// Like `decide`, with caller-supplied observations given to the model for
    /// this one turn. Decides only; a returned capability request is the
    /// caller's to validate and execute.
    pub async fn decide_with_observations(
        &self,
        turn: Turn,
        observations: &[Observation],
    ) -> Result<DecisionReport, AgentError> {
        let step = self.decide_step(turn, observations).await?;
        Ok(DecisionReport {
            turn: step.turn,
            decision: step.decision,
        })
    }

    /// Shared by `decide` and `run_turn`: one model turn, one decision.
    async fn decide_step(
        &self,
        turn: Turn,
        observations: &[Observation],
    ) -> Result<DecideStep, AgentError> {
        let (turn, response) = self.model_turn(turn, observations).await?;
        let mut capability_events = Vec::new();
        let decision = match &self.decision {
            None => Err(DecisionError::InvalidDecision(
                "no decision boundary injected".to_string(),
            )),
            Some(boundary) => {
                let capabilities = match &self.capabilities {
                    None => Ok(Vec::new()),
                    Some(_) => {
                        let report = self.discover_capabilities().await;
                        capability_events = report.events;
                        report.result
                    }
                };
                match capabilities {
                    Ok(capabilities) => boundary.decide(&response, &capabilities),
                    Err(error) => Err(DecisionError::Capability(error)),
                }
            }
        };
        Ok(DecideStep {
            turn,
            response,
            decision,
            capability_events,
        })
    }

    /// The complete single-turn lifecycle: one model call, one decision, and at
    /// most one execution. It never loops, retries, or feeds results back to
    /// the model. A capability that validates but whose execution reports
    /// failure still yields an outcome; `AgentError::Execution` is reserved for
    /// the executor being unable to perform the operation at all.
    pub async fn run_turn(&self, turn: Turn) -> Result<TurnOutcome, AgentError> {
        let message = turn.user_message.clone();
        let step = self.decide_step(turn.clone(), &[]).await?;

        let mut events = vec![AgentEvent::TurnStarted { message }];
        events.extend(
            step.capability_events
                .into_iter()
                .map(AgentEvent::Capability),
        );

        let decision = step.decision.map_err(AgentError::Decision)?;
        events.push(AgentEvent::DecisionMade {
            capability: match &decision {
                AgentDecision::Respond(_) => None,
                AgentDecision::RequestCapability(request) => Some(request.capability_id.clone()),
            },
        });

        let execution = match &decision {
            AgentDecision::Respond(_) => None,
            AgentDecision::RequestCapability(request) => {
                let execution_request = self
                    .validate_capability_request(request)
                    .await
                    .map_err(AgentError::Capability)?;
                let report = self.execute(execution_request).await;
                events.extend(report.events.into_iter().map(AgentEvent::Execution));
                Some(report.result.map_err(AgentError::Execution)?)
            }
        };

        events.push(AgentEvent::TurnCompleted {
            response: step.response.output.clone(),
        });
        Ok(TurnOutcome {
            turn,
            response: step.response,
            decision,
            execution,
            events,
        })
    }

    /// Validates a capability request: declared, available, valid inputs, in that
    /// order. Only then does it yield an `ExecutionRequest`. Never executes.
    pub async fn validate_capability_request(
        &self,
        request: &CapabilityRequest,
    ) -> Result<ExecutionRequest, CapabilityError> {
        let provider = self.capabilities.as_ref().ok_or_else(|| {
            CapabilityError::Unavailable("no capability provider injected".to_string())
        })?;
        let declared = provider.capabilities().await?;
        let descriptor = declared
            .iter()
            .find(|d| d.id == request.capability_id)
            .ok_or_else(|| CapabilityError::Unknown(request.capability_id.to_string()))?;
        match provider.availability(&request.capability_id).await {
            CapabilityAvailability::Available => {}
            CapabilityAvailability::Unavailable(reason)
            | CapabilityAvailability::Misconfigured(reason) => {
                return Err(CapabilityError::Unavailable(reason));
            }
        }
        for name in request.inputs.keys() {
            if !descriptor.inputs.iter().any(|i| &i.name == name) {
                return Err(CapabilityError::InvalidInput(format!(
                    "capability does not accept input '{name}'"
                )));
            }
        }
        // The invocation shape is the capability's: one that declares no inputs takes no `inputs`
        // member at all, so even an empty one is the requester redefining how it is invoked.
        if request.inputs_present && descriptor.inputs.is_empty() {
            return Err(CapabilityError::InvalidInput(
                "capability takes no inputs; the request must not carry an `inputs` member".into(),
            ));
        }
        for input in descriptor.inputs.iter().filter(|i| i.required) {
            if !request.inputs.contains_key(&input.name) {
                return Err(CapabilityError::InvalidInput(format!(
                    "missing required input '{}'",
                    input.name
                )));
            }
        }
        provider
            .validate_inputs(&request.capability_id, &request.inputs)
            .await?;
        Ok(
            ExecutionRequest::for_capability(request.execution_id.clone(), &request.capability_id)
                .with_inputs(request.inputs.clone()),
        )
    }

    /// Validates, then executes, an explicit capability request. A request that
    /// fails validation never reaches the executor.
    pub async fn execute_capability(
        &self,
        request: &CapabilityRequest,
    ) -> Result<ExecutionReport, CapabilityError> {
        let execution = self.validate_capability_request(request).await?;
        Ok(self.execute(execution).await)
    }

    /// Optionally injects a source of capability descriptors.
    pub fn with_capabilities(mut self, provider: Arc<dyn CapabilityProvider>) -> Self {
        self.capabilities = Some(provider);
        self
    }

    /// Asks the injected provider what is available. Only describes; nothing is
    /// selected or executed.
    pub async fn discover_capabilities(&self) -> CapabilityReport {
        let mut events = vec![CapabilityEvent::CapabilitiesRequested];
        let result = match &self.capabilities {
            None => Err(CapabilityError::Unavailable(
                "no capability provider injected".to_string(),
            )),
            Some(provider) => match provider.capabilities().await {
                Err(error) => Err(error),
                Ok(descriptors) => {
                    let mut found = Vec::with_capacity(descriptors.len());
                    for descriptor in descriptors {
                        let availability = provider.availability(&descriptor.id).await;
                        found.push(Capability {
                            descriptor,
                            availability,
                        });
                    }
                    Ok(found)
                }
            },
        };
        events.push(match &result {
            Ok(found) => CapabilityEvent::CapabilitiesAvailable { count: found.len() },
            Err(error) => CapabilityEvent::CapabilitiesUnavailable {
                reason: error.to_string(),
            },
        });
        CapabilityReport { result, events }
    }

    /// Turns an explicitly chosen, declared and available capability into an
    /// execution request. Unknown or unusable capabilities fail here, before
    /// any executor is involved.
    pub async fn request_for_capability(
        &self,
        id: ExecutionId,
        capability: &CapabilityId,
    ) -> Result<ExecutionRequest, CapabilityError> {
        self.validate_capability_request(&CapabilityRequest::new(id, capability.clone()))
            .await
    }

    /// Injects an executor, independent of the model provider.
    pub fn with_executor(mut self, executor: Arc<dyn Executor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Asks the injected executor to perform `request`. Failures stay
    /// execution errors; they are reported, not converted to model errors.
    pub async fn execute(&self, request: ExecutionRequest) -> ExecutionReport {
        let id = request.id.clone();
        let mut events = vec![ExecutionEvent::ExecutionRequested {
            id: id.clone(),
            intent: request.intent.clone(),
        }];

        let outcome = if request.intent.trim().is_empty() {
            Err(ExecutionError::InvalidRequest(
                "intent cannot be empty".to_string(),
            ))
        } else if let Some(executor) = &self.executor {
            events.push(ExecutionEvent::ExecutionStarted { id: id.clone() });
            executor.execute(request).await
        } else {
            Err(ExecutionError::ExecutorUnavailable(
                "no executor injected".to_string(),
            ))
        };

        match &outcome {
            Ok(result) if result.status == ExecutionStatus::Success => {
                events.push(ExecutionEvent::ExecutionCompleted {
                    id,
                    output: result.output.clone(),
                });
            }
            Ok(result) => events.push(ExecutionEvent::ExecutionFailed {
                id,
                reason: result.output.clone(),
            }),
            Err(error) => events.push(ExecutionEvent::ExecutionFailed {
                id,
                reason: error.to_string(),
            }),
        }

        ExecutionReport {
            result: outcome,
            events,
        }
    }

    /// Runs a model turn, then the explicitly supplied execution request.
    /// The caller decides what to execute; the model output is not parsed.
    pub async fn turn_and_execute(
        &self,
        turn: Turn,
        request: ExecutionRequest,
    ) -> Result<AgentResult, AgentError> {
        let turn = self.turn(turn).await?;
        let execution = self.execute(request).await;
        Ok(AgentResult { turn, execution })
    }

    pub async fn turn(&self, turn: Turn) -> Result<TurnResult, AgentError> {
        self.model_turn(turn, &[]).await.map(|(result, _)| result)
    }

    /// One model turn that is explicitly given observations of earlier
    /// executions. Exactly one model call; nothing is executed or observed here.
    pub async fn turn_with_observations(
        &self,
        turn: Turn,
        observations: &[Observation],
    ) -> Result<TurnResult, AgentError> {
        self.model_turn(turn, observations)
            .await
            .map(|(result, _)| result)
    }

    async fn model_turn(
        &self,
        turn: Turn,
        observations: &[Observation],
    ) -> Result<(TurnResult, ModelResponse), AgentError> {
        if turn.user_message.trim().is_empty() {
            return Err(AgentError::InvalidTurn(
                "turn message cannot be empty".to_string(),
            ));
        }

        // Observations first, in the order given, then the user message.
        let mut messages: Vec<Message> = observations
            .iter()
            .map(|observation| Message::new(MessageRole::System, observation.render()))
            .collect();
        messages.push(Message::new(MessageRole::User, turn.user_message.clone()));
        let mut request = ModelRequest::new(self.model.clone(), messages);
        if let Some(tokens) = self.max_output_tokens {
            request.max_tokens = Some(tokens);
        }

        let response = self
            .provider
            .complete(request.clone())
            .await
            .map_err(|error| AgentError::Provider(error.to_string()))?;

        let events = vec![
            AgentEvent::TurnReceived {
                message: turn.user_message.clone(),
            },
            AgentEvent::RequestBuilt {
                model: request.model.to_string(),
            },
            AgentEvent::ModelInvoked {
                model: request.model.to_string(),
            },
            AgentEvent::ModelResponded {
                output: response.output.clone(),
            },
            AgentEvent::TurnCompleted {
                response: response.output.clone(),
            },
        ];

        Ok((
            TurnResult {
                response: response.output.clone(),
                events,
            },
            response,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{Agent, Turn};
    use std::sync::Arc;

    use fx_core::{
        FxError, Message, MessageRole, ModelProvider, ModelRequest, ModelResponse, Usage,
    };

    #[derive(Default)]
    struct TestProvider;

    #[async_trait::async_trait]
    impl ModelProvider for TestProvider {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, FxError> {
            Ok(ModelResponse::new(
                "test-response",
                "Hello from the test provider.",
                Usage::new(4, 6),
            ))
        }
    }

    #[tokio::test]
    async fn agent_turn_returns_response_and_events() {
        let agent = Agent::new(Arc::new(TestProvider));
        let result = agent.turn(Turn::new("Hello")).await.unwrap();

        assert_eq!(result.response, "Hello from the test provider.");
        assert_eq!(result.events.len(), 5);
        assert!(matches!(
            result.events[0],
            crate::AgentEvent::TurnReceived { ref message } if message == "Hello"
        ));
    }
}
