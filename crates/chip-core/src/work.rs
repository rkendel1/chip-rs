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
    /// The one model call an escalation makes has returned (or failed). `usage` is what the
    /// provider reported, `None` when the call failed.
    ModelCalled {
        work_id: WorkId,
        turn: usize,
        usage: Option<ModelUsage>,
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

/// Token usage as the provider reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// Where the wall-clock time went. Latency is observed, never derived from events, and is
/// excluded from every determinism comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkLatency {
    /// Time inside the model call, summed over escalations.
    pub model: Duration,
    /// Time inside the executor, summed over executions.
    pub compute: Duration,
    /// Time in the local policy and the local reasoner (the evidence assessment included).
    pub local_decision: Duration,
    pub total: Duration,
}

/// A stopwatch that does nothing on wasm32, where `Instant::now` is unavailable.
#[derive(Clone, Copy)]
struct Mark {
    #[cfg(not(target_arch = "wasm32"))]
    at: std::time::Instant,
}

impl Mark {
    fn now() -> Mark {
        Mark {
            #[cfg(not(target_arch = "wasm32"))]
            at: std::time::Instant::now(),
        }
    }

    fn elapsed(&self) -> Duration {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.at.elapsed()
        }
        #[cfg(target_arch = "wasm32")]
        {
            Duration::ZERO
        }
    }
}

/// The canonical measurement of one workload. Every count is derived from the event trajectory
/// ([`WorkMeasurement::derive`]) and nothing else, so it cannot disagree with what happened;
/// only the latencies come from outside it. A metric that cannot be measured is an explicit
/// `None`, never an invented value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkMeasurement {
    pub work_id: WorkId,
    pub outcome: WorkOutcome,

    pub turns: u32,
    /// Executions the executor was actually asked to perform.
    pub executions: u32,
    pub observations: u32,

    pub evidence_hits: u32,
    pub local_decisions: u32,
    pub model_escalations: u32,

    /// Bytes and characters sent to the model across all escalations.
    pub context_bytes: u64,
    pub context_chars: u64,

    pub model_calls: u32,
    /// Prompt plus completion tokens the provider reported. `None` when no call reported usage.
    pub model_tokens: Option<u64>,

    pub model_latency: Duration,
    pub compute_latency: Duration,
    /// Local policy and reasoner time. Attribution, not a microbenchmark.
    pub local_decision_latency: Duration,
    pub total_latency: Duration,
}

impl WorkMeasurement {
    /// Counts what the trajectory contains. `executions` counts `ExecutionStarted`, which is
    /// emitted only when the executor is actually invoked.
    pub fn derive(
        work_id: WorkId,
        outcome: WorkOutcome,
        events: &[WorkEvent],
        latency: WorkLatency,
    ) -> WorkMeasurement {
        let mut m = WorkMeasurement {
            work_id,
            outcome,
            turns: 0,
            executions: 0,
            observations: 0,
            evidence_hits: 0,
            local_decisions: 0,
            model_escalations: 0,
            context_bytes: 0,
            context_chars: 0,
            model_calls: 0,
            model_tokens: None,
            model_latency: latency.model,
            compute_latency: latency.compute,
            local_decision_latency: latency.local_decision,
            total_latency: latency.total,
        };
        for event in events {
            match event {
                WorkEvent::DecisionStarted { .. } => m.turns += 1,
                WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. }) => m.executions += 1,
                WorkEvent::ObservationRecorded { .. } => m.observations += 1,
                WorkEvent::EvidenceReused { .. } => m.evidence_hits += 1,
                WorkEvent::LocalDecision { .. } => m.local_decisions += 1,
                WorkEvent::ModelEscalation { context, .. } => {
                    m.model_escalations += 1;
                    m.context_bytes += context.bytes as u64;
                    m.context_chars += context.chars as u64;
                }
                WorkEvent::ModelCalled { usage, .. } => {
                    m.model_calls += 1;
                    if let Some(u) = usage {
                        m.model_tokens = Some(
                            m.model_tokens.unwrap_or(0)
                                + u64::from(u.prompt_tokens)
                                + u64::from(u.completion_tokens),
                        );
                    }
                }
                _ => {}
            }
        }
        m
    }

    pub fn terminal_state(&self) -> TerminalState {
        self.outcome.terminal_state()
    }
}

/// Checks the invariants optimization must never silently break. Returns every violation; an
/// empty list means the trajectory is sound.
///
/// * the run starts with `WorkStarted` and ends with exactly one terminal event;
/// * turns and executions stay within the limits, and turns are numbered in order;
/// * each turn has at most one decision, one capability request and one execution;
/// * a turn that reused evidence performed no execution;
/// * every observation follows an execution in its own turn, and every piece of evidence follows
///   an observation: nothing is believed without an execution;
/// * every escalation is exactly one model call.
pub fn verify_trajectory(events: &[WorkEvent], limits: &WorkLimits) -> Vec<String> {
    let mut violations = Vec::new();
    if !matches!(events.first(), Some(WorkEvent::WorkStarted { .. })) {
        violations.push("the trajectory does not start with WorkStarted".to_string());
    }
    let terminal = |e: &WorkEvent| {
        matches!(
            e,
            WorkEvent::WorkCompleted { .. }
                | WorkEvent::WorkEscalated { .. }
                | WorkEvent::WorkBlocked { .. }
                | WorkEvent::WorkLimitReached { .. }
                | WorkEvent::WorkFailed { .. }
        )
    };
    let terminals = events.iter().filter(|e| terminal(e)).count();
    if terminals != 1 {
        violations.push(format!(
            "expected exactly one terminal event, found {terminals}"
        ));
    }
    if !events.last().is_some_and(terminal) {
        violations.push("the last event is not terminal".to_string());
    }

    #[derive(Default, Clone)]
    struct Turn {
        decision_makers: usize,
        decisions_made: usize,
        requests: usize,
        executions: usize,
        observations: usize,
        evidence_recorded: usize,
        reused: usize,
        escalations: usize,
        model_calls: usize,
    }
    let mut turns: Vec<Turn> = Vec::new();
    let mut current: Option<usize> = None;
    let (mut executions, mut observations) = (0usize, 0usize);
    for event in events {
        match event {
            WorkEvent::DecisionStarted { turn, .. } => {
                if *turn != turns.len() {
                    violations.push(format!(
                        "turn {turn} is out of order (expected {})",
                        turns.len()
                    ));
                }
                turns.push(Turn::default());
                current = Some(turns.len() - 1);
            }
            _ => {
                let Some(i) = current else { continue };
                let t = &mut turns[i];
                match event {
                    WorkEvent::LocalDecision { .. } => t.decision_makers += 1,
                    WorkEvent::ModelEscalation { .. } => {
                        t.decision_makers += 1;
                        t.escalations += 1;
                    }
                    WorkEvent::ModelCalled { .. } => t.model_calls += 1,
                    WorkEvent::DecisionMade { .. } => t.decisions_made += 1,
                    WorkEvent::CapabilityRequested { .. } => t.requests += 1,
                    WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. }) => {
                        t.executions += 1;
                        executions += 1;
                    }
                    WorkEvent::ObservationRecorded { .. } => {
                        if t.executions == 0 {
                            violations.push(format!(
                                "turn {}: an observation without an execution",
                                i + 1
                            ));
                        }
                        t.observations += 1;
                        observations += 1;
                    }
                    WorkEvent::EvidenceRecorded { .. } => {
                        if t.observations == 0 {
                            violations
                                .push(format!("turn {}: evidence without an observation", i + 1));
                        }
                        t.evidence_recorded += 1;
                    }
                    WorkEvent::EvidenceReused { .. } => t.reused += 1,
                    _ => {}
                }
            }
        }
    }
    for (i, t) in turns.iter().enumerate() {
        let n = i + 1;
        if t.decision_makers > 1 || t.decisions_made > 1 {
            violations.push(format!("turn {n}: more than one decision"));
        }
        if t.requests > 1 {
            violations.push(format!("turn {n}: more than one capability request"));
        }
        if t.executions > 1 {
            violations.push(format!("turn {n}: more than one execution"));
        }
        if t.reused > 0 && t.executions > 0 {
            violations.push(format!(
                "turn {n}: evidence was reused and an execution was performed"
            ));
        }
        if t.escalations != t.model_calls {
            violations.push(format!(
                "turn {n}: {} escalations but {} model calls",
                t.escalations, t.model_calls
            ));
        }
    }
    if turns.len() > limits.max_turns {
        violations.push(format!(
            "{} turns exceed the limit of {}",
            turns.len(),
            limits.max_turns
        ));
    }
    if executions > limits.max_executions {
        violations.push(format!(
            "{executions} executions exceed the limit of {}",
            limits.max_executions
        ));
    }
    if observations > executions {
        violations.push(format!(
            "{observations} observations from {executions} executions"
        ));
    }
    violations
}

/// One iteration, structurally: what was decided and by whom, whether anything was executed,
/// observed or reused, and what an escalation sent. There is no prompt text here, only shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnTrace {
    /// Zero-based.
    pub turn: usize,
    /// `RequestCapability`, `Complete`, `Escalate` or `Block`; `None` if the turn ended before a
    /// decision (a failed escalation).
    pub decision: Option<&'static str>,
    pub capability: Option<CapabilityId>,
    pub source: Option<DecisionSource>,
    /// The executor was invoked this turn.
    pub executed: bool,
    pub evidence_reused: bool,
    pub evidence_recorded: bool,
    /// `None` when nothing was observed or reused; otherwise whether a receipt was present.
    pub receipt_present: Option<bool>,
    pub observed: bool,
    /// Present when this turn escalated to the model.
    pub context: Option<ContextMetrics>,
    pub escalation_reason: Option<String>,
    pub model_called: bool,
}

/// The structural trajectory of a run, turn by turn, derived from its events.
pub fn trace(events: &[WorkEvent]) -> Vec<TurnTrace> {
    let mut turns: Vec<TurnTrace> = Vec::new();
    for event in events {
        if let WorkEvent::DecisionStarted { turn, .. } = event {
            turns.push(TurnTrace {
                turn: *turn,
                decision: None,
                capability: None,
                source: None,
                executed: false,
                evidence_reused: false,
                evidence_recorded: false,
                receipt_present: None,
                observed: false,
                context: None,
                escalation_reason: None,
                model_called: false,
            });
            continue;
        }
        let Some(t) = turns.last_mut() else { continue };
        match event {
            WorkEvent::LocalDecision { .. } => t.source = Some(DecisionSource::Local),
            WorkEvent::ModelEscalation {
                reason, context, ..
            } => {
                t.source = Some(DecisionSource::Model);
                t.context = Some(*context);
                t.escalation_reason = Some(reason.clone());
            }
            WorkEvent::ModelCalled { .. } => t.model_called = true,
            WorkEvent::DecisionMade { decision, .. } => {
                t.decision = Some(match decision.split(' ').next() {
                    Some("request") => "RequestCapability",
                    Some("complete") => "Complete",
                    Some("escalate") => "Escalate",
                    _ => "Block",
                });
            }
            WorkEvent::CapabilityRequested { capability, .. } => {
                t.capability = Some(capability.clone())
            }
            WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. }) => t.executed = true,
            WorkEvent::ObservationRecorded { receipt_id, .. } => {
                t.observed = true;
                t.receipt_present = Some(receipt_id.is_some());
            }
            WorkEvent::EvidenceRecorded { .. } => t.evidence_recorded = true,
            WorkEvent::EvidenceReused { receipt_id, .. } => {
                t.evidence_reused = true;
                t.receipt_present = Some(receipt_id.is_some());
            }
            _ => {}
        }
    }
    turns
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
    /// Observed wall-clock attribution.
    pub latency: WorkLatency,
}

impl WorkReport {
    /// The structural trajectory, turn by turn.
    pub fn trace(&self) -> Vec<TurnTrace> {
        trace(&self.events)
    }

    /// The canonical measurement, derived from this report's event trajectory.
    pub fn measurement(&self) -> WorkMeasurement {
        WorkMeasurement::derive(
            self.work_id.clone(),
            self.outcome.clone(),
            &self.events,
            self.latency,
        )
    }
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
    latency: WorkLatency,
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
        let started = Mark::now();
        let called = self
            .agent
            .model_turn(Turn::new(context.render()), &context.relevant_observations)
            .await;
        self.latency.model += started.elapsed();
        self.events.push(WorkEvent::ModelCalled {
            work_id: self.id(),
            turn,
            usage: called.as_ref().ok().map(|(_, r)| ModelUsage {
                prompt_tokens: r.usage.prompt_tokens,
                completion_tokens: r.usage.completion_tokens,
            }),
        });
        let (_, response) = called.map_err(|e| WorkOutcome::Failed {
            reason: format!("model escalation failed: {e}"),
        })?;
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
        let started = Mark::now();
        let report = self.agent.execute(execution).await;
        self.latency.compute += started.elapsed();
        // An execution counts when the executor was actually invoked, which is what
        // `ExecutionStarted` records; an unavailable executor performed nothing.
        if report
            .events
            .iter()
            .any(|e| matches!(e, ExecutionEvent::ExecutionStarted { .. }))
        {
            self.summary.executions += 1;
        }
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
        let started = Mark::now();
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
            latency: WorkLatency::default(),
        };

        let outcome = self.drive(&mut run, policy, boundary).await;

        run.events.push(terminal_event(spec.id.clone(), &outcome));
        run.latency.total = started.elapsed();
        let latency = run.latency;

        // The summary is the measurement, derived from the events. The counters the loop kept
        // while running (they drive its limits) must agree with it.
        let measurement =
            WorkMeasurement::derive(spec.id.clone(), outcome.clone(), &run.events, latency);
        debug_assert_eq!(measurement.turns as usize, run.summary.turns);
        debug_assert_eq!(measurement.executions as usize, run.summary.executions);
        debug_assert_eq!(measurement.observations as usize, run.summary.observations);
        debug_assert_eq!(
            measurement.evidence_hits as usize,
            run.summary.evidence_hits
        );
        debug_assert_eq!(
            measurement.local_decisions as usize,
            run.summary.local_decisions
        );
        debug_assert_eq!(
            measurement.model_escalations as usize,
            run.summary.model_escalations
        );
        debug_assert_eq!(
            measurement.context_bytes as usize,
            run.summary.context_bytes
        );
        let tokens = events_usage(&run.events);
        let summary = WorkSummary {
            turns: measurement.turns as usize,
            executions: measurement.executions as usize,
            observations: measurement.observations as usize,
            evidence_hits: measurement.evidence_hits as usize,
            local_decisions: measurement.local_decisions as usize,
            model_escalations: measurement.model_escalations as usize,
            context_bytes: measurement.context_bytes as usize,
            prompt_tokens: tokens.0,
            completion_tokens: tokens.1,
            elapsed: latency.total,
            terminal_state: outcome.terminal_state(),
        };
        WorkReport {
            work_id: spec.id.clone(),
            outcome,
            summary,
            events: run.events,
            decisions: run.decisions,
            observations: run.observations,
            escalations: run.escalations,
            latency,
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
            let local_started = Mark::now();
            let proposal = policy.propose(&run.view(turn));
            run.latency.local_decision += local_started.elapsed();
            let mut already_checked = false;
            let (decision, escalation) = match proposal {
                Some(WorkDecision::RequestCapability(request)) => {
                    already_checked = true;
                    let assess_started = Mark::now();
                    let assessed = self.assess_evidence(&request, spec.state.as_ref());
                    run.latency.local_decision += assess_started.elapsed();
                    match assessed {
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

/// Prompt and completion tokens the provider reported, summed over the trajectory.
fn events_usage(events: &[WorkEvent]) -> (u64, u64) {
    events.iter().fold((0, 0), |(p, c), e| match e {
        WorkEvent::ModelCalled { usage: Some(u), .. } => (
            p + u64::from(u.prompt_tokens),
            c + u64::from(u.completion_tokens),
        ),
        _ => (p, c),
    })
}
