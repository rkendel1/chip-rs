//! The bounded autonomous work loop.
//!
//! One decision per iteration: a decision may complete the work, escalate it, block it, or request
//! exactly one capability. A request becomes at most one execution, one observation and, where
//! the rules allow, one piece of evidence, and the loop then makes the next decision without the
//! caller starting another turn. The loop is finite by construction: [`WorkLimits`] bound both
//! turns and executions, and every run ends in an explicit [`WorkOutcome`].
//!
//! Where each decision comes from, in order:
//!
//! 1. **Local.** A [`LocalWorkPolicy`] may propose the next decision. A proposed capability
//!    request is first assessed with the evidence machinery (`Agent::assess_evidence`): valid
//!    evidence is reused and nothing is executed; otherwise the local reasoner either lets the
//!    request proceed or escalates. The PR25 learned model is not wired in anywhere: it is not an
//!    authority for autonomous `Continue`.
//! 2. **Model (FX).** When the local side cannot decide, the loop builds an [`EscalationContext`],
//!    emits a `ModelEscalation` event carrying its measurements, calls the model exactly once and
//!    interprets the response through a [`WorkDecisionBoundary`]. There is no retry.
//!
//! Reality comes only from the execution and observation path. A capability request, a model
//! response or a plan is never evidence that anything happened: only an observation of an
//! execution this loop performed is recorded as evidence, and a cancelled execution never is.
//!
//! The loop discovers nothing: capabilities are described once, when the work starts. It scans
//! no repository, builds no graph, reads no history from storage and opens no connection beyond
//! the one model call an escalation makes.

use std::fmt;
use std::time::Duration;

use fx_core::ModelResponse;

use crate::{
    Agent, AgentDecision, AgentError, Assessment, Capability, CapabilityEvent, CapabilityId,
    CapabilityRequest, DecisionBoundary, DecisionError, EvidenceLookup, ExecutionError,
    ExecutionEvent, ExecutionId, Observation, ObservationKind, ReasoningError, StateToken, Turn,
};

/// Opaque identity of one workload. It carries no meaning.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkId(String);

impl WorkId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the work is for. A plain string; there is no goal language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkGoal(String);

impl WorkGoal {
    pub fn new(goal: impl Into<String>) -> Self {
        Self(goal.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Explicit, finite bounds. There is no way to run without them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkLimits {
    /// Most decisions (iterations) the work may make.
    pub max_turns: usize,
    /// Most executions the work may perform.
    pub max_executions: usize,
}

impl Default for WorkLimits {
    fn default() -> Self {
        Self {
            max_turns: 8,
            max_executions: 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    Turns,
    Executions,
}

impl LimitKind {
    pub fn name(self) -> &'static str {
        match self {
            LimitKind::Turns => "turns",
            LimitKind::Executions => "executions",
        }
    }
}

/// What one iteration decided. `RequestCapability` is data: it is not execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkDecision {
    /// The goal is complete.
    Complete { summary: String },
    /// Intervention outside this bounded loop is required.
    Escalate { reason: String },
    /// The work cannot proceed: something it requires is unavailable.
    Block { reason: String },
    /// Ask for exactly one capability.
    RequestCapability(CapabilityRequest),
}

impl WorkDecision {
    /// A short, secret-free label for events and context.
    pub fn label(&self) -> String {
        match self {
            WorkDecision::Complete { .. } => "complete".to_string(),
            WorkDecision::Escalate { .. } => "escalate".to_string(),
            WorkDecision::Block { .. } => "block".to_string(),
            WorkDecision::RequestCapability(r) => format!("request {}", r.capability_id),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    Local,
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRecord {
    pub turn: usize,
    pub source: DecisionSource,
    pub decision: WorkDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalState {
    Completed,
    Escalated,
    Blocked,
    LimitReached,
    Failed,
}

impl TerminalState {
    pub fn name(self) -> &'static str {
        match self {
            TerminalState::Completed => "completed",
            TerminalState::Escalated => "escalated",
            TerminalState::Blocked => "blocked",
            TerminalState::LimitReached => "limit_reached",
            TerminalState::Failed => "failed",
        }
    }
}

/// How the work ended. Limits and failures are never reported as success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkOutcome {
    Completed { summary: String },
    Escalated { reason: String },
    Blocked { reason: String },
    LimitReached { limit: LimitKind },
    Failed { reason: String },
}

impl WorkOutcome {
    pub fn terminal_state(&self) -> TerminalState {
        match self {
            WorkOutcome::Completed { .. } => TerminalState::Completed,
            WorkOutcome::Escalated { .. } => TerminalState::Escalated,
            WorkOutcome::Blocked { .. } => TerminalState::Blocked,
            WorkOutcome::LimitReached { .. } => TerminalState::LimitReached,
            WorkOutcome::Failed { .. } => TerminalState::Failed,
        }
    }
}

/// What a local policy may look at. Everything is already in memory.
#[derive(Debug, Clone, Copy)]
pub struct WorkView<'a> {
    pub goal: &'a WorkGoal,
    /// Zero-based index of the iteration being decided.
    pub turn: usize,
    pub executions: usize,
    /// Observations so far: of executions the loop performed, and of evidence it reused.
    pub observations: &'a [Observation],
    pub decisions: &'a [DecisionRecord],
}

/// Supplies the next decision when Chip can decide locally. Pure and synchronous: it sees only
/// the view. `None` means "I cannot", and the loop escalates to the model.
pub trait LocalWorkPolicy: Send + Sync {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision>;
}

/// A policy that never decides: every iteration escalates to the model.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoLocalPolicy;

impl LocalWorkPolicy for NoLocalPolicy {
    fn propose(&self, _view: &WorkView<'_>) -> Option<WorkDecision> {
        None
    }
}

/// Deterministic policy for tests and demos: the decision for iteration `n` is entry `n`;
/// `None` (or running past the end) escalates.
#[derive(Debug, Clone, Default)]
pub struct ScriptedPolicy {
    decisions: Vec<Option<WorkDecision>>,
}

impl ScriptedPolicy {
    pub fn new(decisions: Vec<Option<WorkDecision>>) -> Self {
        Self { decisions }
    }
}

impl LocalWorkPolicy for ScriptedPolicy {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        self.decisions.get(view.turn).cloned().flatten()
    }
}

/// Interprets a model response as a work decision. Pure and synchronous: model output is data,
/// and only this boundary decides what, if anything, it means.
pub trait WorkDecisionBoundary: Send + Sync {
    fn interpret(
        &self,
        response: &ModelResponse,
        capabilities: &[Capability],
    ) -> Result<WorkDecision, DecisionError>;
}

/// Adapts the existing [`DecisionBoundary`]: a capability request stays a request, and a plain
/// response is the model saying the work is done. Neither is evidence that anything occurred.
pub struct RespondCompletes<B: DecisionBoundary>(pub B);

impl<B: DecisionBoundary> WorkDecisionBoundary for RespondCompletes<B> {
    fn interpret(
        &self,
        response: &ModelResponse,
        capabilities: &[Capability],
    ) -> Result<WorkDecision, DecisionError> {
        Ok(match self.0.decide(response, capabilities)? {
            AgentDecision::Respond(r) => WorkDecision::Complete { summary: r.output },
            AgentDecision::RequestCapability(r) => WorkDecision::RequestCapability(r),
        })
    }
}

/// What goes into one model escalation, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscalationContext {
    pub goal: String,
    pub current_state: String,
    /// What the evidence store said about the candidate this escalation is about, if any.
    pub relevant_evidence: Vec<String>,
    /// Observations so far. Handed to the model as the existing observation messages.
    pub relevant_observations: Vec<Observation>,
    pub prior_decisions: Vec<String>,
    /// Capabilities already tried and known to have failed.
    pub ruled_out: Vec<String>,
    pub question: String,
}

/// Measurements of one escalation's context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextMetrics {
    /// Bytes of every message sent: the rendered context plus each rendered observation.
    pub bytes: usize,
    pub chars: usize,
    pub observations: usize,
    pub decisions: usize,
    pub evidence_items: usize,
    pub ruled_out: usize,
}

impl EscalationContext {
    /// The text of the user message. Observations are not in it; they travel as their own
    /// messages.
    pub fn render(&self) -> String {
        let mut out = format!("Goal: {}\nState: {}\n", self.goal, self.current_state);
        out.push_str("Evidence:\n");
        for item in &self.relevant_evidence {
            out.push_str(&format!("  - {item}\n"));
        }
        out.push_str("Prior decisions:\n");
        for item in &self.prior_decisions {
            out.push_str(&format!("  - {item}\n"));
        }
        out.push_str("Ruled out:\n");
        for item in &self.ruled_out {
            out.push_str(&format!("  - {item}\n"));
        }
        out.push_str(&format!("Question: {}", self.question));
        out
    }

    pub fn metrics(&self) -> ContextMetrics {
        let text = self.render();
        let rendered: Vec<String> = self
            .relevant_observations
            .iter()
            .map(Observation::render)
            .collect();
        ContextMetrics {
            bytes: text.len() + rendered.iter().map(String::len).sum::<usize>(),
            chars: text.chars().count() + rendered.iter().map(|r| r.chars().count()).sum::<usize>(),
            observations: self.relevant_observations.len(),
            decisions: self.prior_decisions.len(),
            evidence_items: self.relevant_evidence.len(),
            ruled_out: self.ruled_out.len(),
        }
    }
}

/// The ordered trajectory. Execution and capability events are the existing ones, wrapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkEvent {
    WorkStarted {
        work_id: WorkId,
        goal: String,
        limits: WorkLimits,
    },
    Capability(CapabilityEvent),
    DecisionStarted {
        work_id: WorkId,
        turn: usize,
    },
    /// The decision was made without the model.
    LocalDecision {
        work_id: WorkId,
        turn: usize,
        decision: String,
    },
    /// The expensive boundary: one model call is about to be made. Carries measurements, not the
    /// prompt.
    ModelEscalation {
        work_id: WorkId,
        turn: usize,
        reason: String,
        context: ContextMetrics,
    },
    DecisionMade {
        work_id: WorkId,
        turn: usize,
        decision: String,
    },
    CapabilityRequested {
        work_id: WorkId,
        turn: usize,
        capability: CapabilityId,
    },
    /// ExecutionRequested / ExecutionStarted / ExecutionCompleted / ExecutionFailed.
    Execution(ExecutionEvent),
    ObservationRecorded {
        work_id: WorkId,
        turn: usize,
        execution_id: ExecutionId,
        kind: ObservationKind,
        receipt_id: Option<String>,
    },
    EvidenceRecorded {
        work_id: WorkId,
        turn: usize,
        capability: CapabilityId,
    },
    EvidenceReused {
        work_id: WorkId,
        turn: usize,
        capability: CapabilityId,
        receipt_id: Option<String>,
    },
    WorkCompleted {
        work_id: WorkId,
    },
    WorkEscalated {
        work_id: WorkId,
        reason: String,
    },
    WorkBlocked {
        work_id: WorkId,
        reason: String,
    },
    WorkLimitReached {
        work_id: WorkId,
        limit: LimitKind,
    },
    WorkFailed {
        work_id: WorkId,
        reason: String,
    },
}

/// A baseline, not an optimization target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkSummary {
    pub turns: usize,
    pub executions: usize,
    /// Observations of executions this loop performed.
    pub observations: usize,
    pub evidence_hits: usize,
    pub local_decisions: usize,
    pub model_escalations: usize,
    /// Bytes sent to the model across all escalations.
    pub context_bytes: usize,
    /// Token usage the provider reported, summed. Zero when no escalation happened.
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Wall-clock time. Not deterministic; excluded from determinism comparisons.
    pub elapsed: Duration,
    pub terminal_state: TerminalState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkReport {
    pub work_id: WorkId,
    pub outcome: WorkOutcome,
    pub summary: WorkSummary,
    pub events: Vec<WorkEvent>,
    pub decisions: Vec<DecisionRecord>,
    pub observations: Vec<Observation>,
    /// The measurements of every escalation, in order.
    pub escalations: Vec<ContextMetrics>,
}

/// Everything `Agent::run_work` needs to know about one workload.
#[derive(Debug, Clone)]
pub struct WorkSpec {
    pub id: WorkId,
    pub goal: WorkGoal,
    pub limits: WorkLimits,
    /// The current state evidence must have been established under, if the caller tracks one.
    pub state: Option<StateToken>,
}

impl WorkSpec {
    pub fn new(id: WorkId, goal: WorkGoal) -> Self {
        Self {
            id,
            goal,
            limits: WorkLimits::default(),
            state: None,
        }
    }

    pub fn with_limits(mut self, limits: WorkLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_state(mut self, state: StateToken) -> Self {
        self.state = Some(state);
        self
    }
}

enum Step {
    Next,
    Done(WorkOutcome),
}

struct Run<'a> {
    agent: &'a Agent,
    spec: &'a WorkSpec,
    events: Vec<WorkEvent>,
    observations: Vec<Observation>,
    decisions: Vec<DecisionRecord>,
    ruled_out: Vec<String>,
    escalations: Vec<ContextMetrics>,
    capabilities: Vec<Capability>,
    summary: WorkSummary,
}

impl<'a> Run<'a> {
    fn id(&self) -> WorkId {
        self.spec.id.clone()
    }

    fn view(&self, turn: usize) -> WorkView<'_> {
        WorkView {
            goal: &self.spec.goal,
            turn,
            executions: self.summary.executions,
            observations: &self.observations,
            decisions: &self.decisions,
        }
    }

    fn decide_locally(&mut self, turn: usize, decision: &WorkDecision) {
        self.summary.local_decisions += 1;
        self.events.push(WorkEvent::LocalDecision {
            work_id: self.id(),
            turn,
            decision: decision.label(),
        });
        self.decisions.push(DecisionRecord {
            turn,
            source: DecisionSource::Local,
            decision: decision.clone(),
        });
    }

    /// Reuses valid evidence: nothing is executed, and the observation joins the state.
    fn reuse(&mut self, turn: usize, request: &CapabilityRequest, observation: Observation) {
        self.summary.evidence_hits += 1;
        self.events.push(WorkEvent::EvidenceReused {
            work_id: self.id(),
            turn,
            capability: request.capability_id.clone(),
            receipt_id: observation.receipt_id.clone(),
        });
        self.observations.push(observation);
    }

    fn context(&self, turn: usize, evidence: Vec<String>) -> EscalationContext {
        EscalationContext {
            goal: self.spec.goal.as_str().to_string(),
            current_state: format!(
                "turn {} of {}; executions {} of {}",
                turn + 1,
                self.spec.limits.max_turns,
                self.summary.executions,
                self.spec.limits.max_executions
            ),
            relevant_evidence: evidence,
            relevant_observations: self.observations.clone(),
            prior_decisions: self
                .decisions
                .iter()
                .map(|d| {
                    format!(
                        "turn {} ({}): {}",
                        d.turn + 1,
                        if d.source == DecisionSource::Local {
                            "local"
                        } else {
                            "model"
                        },
                        d.decision.label()
                    )
                })
                .collect(),
            ruled_out: self.ruled_out.clone(),
            question:
                "Decide the next step: respond if the goal is complete, or request one capability."
                    .to_string(),
        }
    }

    /// Makes the one model call an escalation allows, and interprets its response.
    async fn escalate(
        &mut self,
        turn: usize,
        reason: String,
        evidence: Vec<String>,
        boundary: &dyn WorkDecisionBoundary,
    ) -> Result<WorkDecision, WorkOutcome> {
        let context = self.context(turn, evidence);
        let metrics = context.metrics();
        self.summary.model_escalations += 1;
        self.summary.context_bytes += metrics.bytes;
        self.escalations.push(metrics);
        self.events.push(WorkEvent::ModelEscalation {
            work_id: self.id(),
            turn,
            reason,
            context: metrics,
        });
        let (_, response) = self
            .agent
            .model_turn(Turn::new(context.render()), &context.relevant_observations)
            .await
            .map_err(|e| WorkOutcome::Failed {
                reason: format!("model escalation failed: {e}"),
            })?;
        self.summary.prompt_tokens += u64::from(response.usage.prompt_tokens);
        self.summary.completion_tokens += u64::from(response.usage.completion_tokens);
        let decision = boundary
            .interpret(&response, &self.capabilities)
            .map_err(|e| WorkOutcome::Failed {
                reason: format!("the model's response is not a valid decision: {e}"),
            })?;
        self.decisions.push(DecisionRecord {
            turn,
            source: DecisionSource::Model,
            decision: decision.clone(),
        });
        Ok(decision)
    }

    fn lookup(&self, request: &CapabilityRequest) -> EvidenceLookup {
        match &self.spec.state {
            Some(state) => self.agent.lookup_valid_evidence(request, state),
            None => self.agent.lookup_evidence(request),
        }
    }

    /// Validate, execute once, observe, and record. The only way anything comes to be believed.
    async fn perform(&mut self, turn: usize, request: &CapabilityRequest) -> Step {
        if self.summary.executions >= self.spec.limits.max_executions {
            return Step::Done(WorkOutcome::LimitReached {
                limit: LimitKind::Executions,
            });
        }
        let execution = match self.agent.validate_capability_request(request).await {
            Ok(execution) => execution,
            Err(e) => {
                return Step::Done(WorkOutcome::Blocked {
                    reason: e.to_string(),
                });
            }
        };
        self.summary.executions += 1;
        let report = self.agent.execute(execution).await;
        self.events
            .extend(report.events.into_iter().map(WorkEvent::Execution));
        let result = match report.result {
            Ok(result) => result,
            Err(ExecutionError::ExecutorUnavailable(reason)) => {
                return Step::Done(WorkOutcome::Blocked { reason });
            }
            Err(e) => {
                return Step::Done(WorkOutcome::Failed {
                    reason: e.to_string(),
                });
            }
        };
        let observation = match self.agent.observe(&result) {
            Ok(observation) => observation,
            Err(e) => {
                return Step::Done(WorkOutcome::Failed {
                    reason: e.to_string(),
                });
            }
        };
        self.summary.observations += 1;
        self.events.push(WorkEvent::ObservationRecorded {
            work_id: self.id(),
            turn,
            execution_id: observation.execution_id.clone(),
            kind: observation.kind,
            receipt_id: observation.receipt_id.clone(),
        });
        let recorded = match &self.spec.state {
            Some(state) => self
                .agent
                .record_evidence_under(request, &observation, state),
            None => self.agent.record_evidence(request, &observation),
        };
        match recorded {
            Ok(true) => self.events.push(WorkEvent::EvidenceRecorded {
                work_id: self.id(),
                turn,
                capability: request.capability_id.clone(),
            }),
            Ok(false) => {}
            Err(e) => {
                return Step::Done(WorkOutcome::Failed {
                    reason: e.to_string(),
                });
            }
        }
        if observation.status == crate::ExecutionStatus::Failure {
            self.ruled_out.push(format!(
                "capability {}: its execution failed",
                request.capability_id
            ));
        }
        self.observations.push(observation);
        Step::Next
    }
}

fn terminal_event(work_id: WorkId, outcome: &WorkOutcome) -> WorkEvent {
    match outcome {
        WorkOutcome::Completed { .. } => WorkEvent::WorkCompleted { work_id },
        WorkOutcome::Escalated { reason } => WorkEvent::WorkEscalated {
            work_id,
            reason: reason.clone(),
        },
        WorkOutcome::Blocked { reason } => WorkEvent::WorkBlocked {
            work_id,
            reason: reason.clone(),
        },
        WorkOutcome::LimitReached { limit } => WorkEvent::WorkLimitReached {
            work_id,
            limit: *limit,
        },
        WorkOutcome::Failed { reason } => WorkEvent::WorkFailed {
            work_id,
            reason: reason.clone(),
        },
    }
}

impl Agent {
    /// Runs one bounded autonomous workload to a terminal outcome. See the module documentation.
    pub async fn run_work(
        &self,
        spec: &WorkSpec,
        policy: &dyn LocalWorkPolicy,
        boundary: &dyn WorkDecisionBoundary,
    ) -> WorkReport {
        #[cfg(not(target_arch = "wasm32"))]
        let started = std::time::Instant::now();
        let mut run = Run {
            agent: self,
            spec,
            events: vec![WorkEvent::WorkStarted {
                work_id: spec.id.clone(),
                goal: spec.goal.as_str().to_string(),
                limits: spec.limits,
            }],
            observations: Vec::new(),
            decisions: Vec::new(),
            ruled_out: Vec::new(),
            escalations: Vec::new(),
            capabilities: Vec::new(),
            summary: WorkSummary {
                turns: 0,
                executions: 0,
                observations: 0,
                evidence_hits: 0,
                local_decisions: 0,
                model_escalations: 0,
                context_bytes: 0,
                prompt_tokens: 0,
                completion_tokens: 0,
                elapsed: Duration::ZERO,
                terminal_state: TerminalState::Failed,
            },
        };

        let outcome = self.drive(&mut run, policy, boundary).await;

        run.events.push(terminal_event(spec.id.clone(), &outcome));
        run.summary.terminal_state = outcome.terminal_state();
        #[cfg(not(target_arch = "wasm32"))]
        {
            run.summary.elapsed = started.elapsed();
        }
        WorkReport {
            work_id: spec.id.clone(),
            outcome,
            summary: run.summary,
            events: run.events,
            decisions: run.decisions,
            observations: run.observations,
            escalations: run.escalations,
        }
    }

    async fn drive(
        &self,
        run: &mut Run<'_>,
        policy: &dyn LocalWorkPolicy,
        boundary: &dyn WorkDecisionBoundary,
    ) -> WorkOutcome {
        let spec = run.spec;
        if spec.goal.as_str().trim().is_empty() {
            return WorkOutcome::Failed {
                reason: "the goal cannot be empty".to_string(),
            };
        }

        // Capabilities are described once, here; the loop never discovers again.
        if self.capabilities.is_some() {
            let report = self.discover_capabilities().await;
            run.events
                .extend(report.events.into_iter().map(WorkEvent::Capability));
            match report.result {
                Ok(found) => run.capabilities = found,
                Err(e) => {
                    return WorkOutcome::Blocked {
                        reason: e.to_string(),
                    };
                }
            }
        }

        loop {
            if run.summary.turns >= spec.limits.max_turns {
                return WorkOutcome::LimitReached {
                    limit: LimitKind::Turns,
                };
            }
            let turn = run.summary.turns;
            run.summary.turns += 1;
            run.events.push(WorkEvent::DecisionStarted {
                work_id: spec.id.clone(),
                turn,
            });

            // 1. Local: a proposal, assessed by the evidence machinery when it is a request.
            let proposal = policy.propose(&run.view(turn));
            let mut already_checked = false;
            let (decision, escalation) = match proposal {
                Some(WorkDecision::RequestCapability(request)) => {
                    already_checked = true;
                    match self.assess_evidence(&request, spec.state.as_ref()) {
                        Ok(Assessment::Reuse(observation)) => {
                            let decision = WorkDecision::RequestCapability(request.clone());
                            run.decide_locally(turn, &decision);
                            run.events.push(WorkEvent::DecisionMade {
                                work_id: spec.id.clone(),
                                turn,
                                decision: decision.label(),
                            });
                            run.events.push(WorkEvent::CapabilityRequested {
                                work_id: spec.id.clone(),
                                turn,
                                capability: request.capability_id.clone(),
                            });
                            run.reuse(turn, &request, observation);
                            continue;
                        }
                        Ok(Assessment::Continue { .. }) => {
                            let decision = WorkDecision::RequestCapability(request);
                            run.decide_locally(turn, &decision);
                            (Some(decision), None)
                        }
                        Ok(Assessment::Escalate { reason }) => {
                            let evidence = vec![format!(
                                "{}: no valid evidence (absent or stale)",
                                request.capability_id
                            )];
                            (None, Some((format!("local reasoner: {reason}"), evidence)))
                        }
                        Err(AgentError::Reasoning(ReasoningError::Unavailable(_))) => (
                            None,
                            Some(("no local reasoner is available".to_string(), Vec::new())),
                        ),
                        Err(e) => {
                            return WorkOutcome::Failed {
                                reason: e.to_string(),
                            };
                        }
                    }
                }
                Some(terminal) => {
                    run.decide_locally(turn, &terminal);
                    (Some(terminal), None)
                }
                None => (
                    None,
                    Some(("no local decision is available".to_string(), Vec::new())),
                ),
            };

            // 2. Model: exactly one call per escalation, no retry.
            let decision = match (decision, escalation) {
                (Some(decision), _) => decision,
                (None, Some((reason, evidence))) => {
                    match run.escalate(turn, reason, evidence, boundary).await {
                        Ok(decision) => decision,
                        Err(outcome) => return outcome,
                    }
                }
                (None, None) => unreachable!("a decision or an escalation is always produced"),
            };
            run.events.push(WorkEvent::DecisionMade {
                work_id: spec.id.clone(),
                turn,
                decision: decision.label(),
            });

            match decision {
                WorkDecision::Complete { summary } => return WorkOutcome::Completed { summary },
                WorkDecision::Escalate { reason } => return WorkOutcome::Escalated { reason },
                WorkDecision::Block { reason } => return WorkOutcome::Blocked { reason },
                WorkDecision::RequestCapability(request) => {
                    run.events.push(WorkEvent::CapabilityRequested {
                        work_id: spec.id.clone(),
                        turn,
                        capability: request.capability_id.clone(),
                    });
                    // A request the model made gets the same evidence check a local one got.
                    if !already_checked {
                        if let EvidenceLookup::Found(observation) = run.lookup(&request) {
                            run.reuse(turn, &request, observation);
                            continue;
                        }
                    }
                    match run.perform(turn, &request).await {
                        Step::Next => {}
                        Step::Done(outcome) => return outcome,
                    }
                }
            }
        }
    }
}
