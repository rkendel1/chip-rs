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
use std::sync::Arc;
use std::time::Duration;

use fx_core::ModelResponse;

use crate::{
    Agent, AgentDecision, AgentError, Assessment, Capability, CapabilityEvent, CapabilityId,
    CapabilityRequest, DecisionBoundary, DecisionError, EvidenceLookup, ExecutionError,
    ExecutionEvent, ExecutionId, InputValue, Observation, ObservationKind, ReasoningError,
    StateToken, Turn,
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
    /// The next model request would have exceeded the configured context budget.
    Context,
}

impl LimitKind {
    pub fn name(self) -> &'static str {
        match self {
            LimitKind::Turns => "turns",
            LimitKind::Executions => "executions",
            LimitKind::Context => "context",
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

    /// What the model is asked, given the capabilities that were described when the work started.
    /// It is part of the escalation context and counted in its measurements. A boundary that
    /// expects a particular reply format says so here.
    fn question(&self, _capabilities: &[Capability]) -> String {
        DEFAULT_QUESTION.to_string()
    }
}

const DEFAULT_QUESTION: &str =
    "Decide the next step: respond if the goal is complete, or request one capability.";

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
    /// Capabilities already tried and known to have failed, or to have run without satisfying the
    /// goal.
    pub ruled_out: Vec<String>,
    /// Observations left out of this request because a later, identical observation is in it. Each
    /// entry is a fact Chip established; nothing here is a summary of what an observation said.
    pub omitted: Vec<String>,
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
    /// The request's messages by role: each observation travels as one system message, the
    /// rendered context as one user message. Never an assistant message.
    pub messages: usize,
    pub system_messages: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    /// Observations the request leaves out because a later identical one is in it.
    pub omitted_observations: usize,
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
        if !self.omitted.is_empty() {
            out.push_str("Omitted (identical to a later observation):\n");
            for item in &self.omitted {
                out.push_str(&format!("  - {item}\n"));
            }
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
            messages: self.relevant_observations.len() + 1,
            system_messages: self.relevant_observations.len(),
            user_messages: 1,
            assistant_messages: 0,
            omitted_observations: self.omitted.len(),
        }
    }
}

/// Where the workload stands, as the loop knows it.
#[derive(Debug, Clone, Copy)]
pub struct WorkState<'a> {
    pub goal: &'a str,
    /// Zero-based turn of the decision being made.
    pub turn: usize,
    pub max_turns: usize,
    pub executions: usize,
    pub max_executions: usize,
}

/// What has happened so far, plus what this escalation is about.
#[derive(Debug, Clone, Copy)]
pub struct WorkTrajectory<'a> {
    pub observations: &'a [Observation],
    /// Where each observation came from, one per observation, in the same order.
    pub origins: &'a [ObservationOrigin],
    pub decisions: &'a [DecisionRecord],
    /// Capabilities already tried and known to have failed.
    pub ruled_out: &'a [String],
    /// What the evidence store said about the candidate this escalation is about.
    pub evidence: &'a [String],
    /// The boundary's question, which states the reply contract.
    pub question: &'a str,
}

/// Chooses what one model escalation is told. Chip's opinion about relevance lives here, not in
/// FX and not in the loop.
///
/// A policy is a pure function of what it is given: it cannot execute anything, observe anything
/// or reach a provider, and it does not measure itself. Measurement counts the request that is
/// actually serialized for the provider.
pub trait EscalationContextPolicy: Send + Sync {
    /// A stable, non-secret identifier (for example `full-v1`), recorded with every escalation.
    fn id(&self) -> &'static str;

    fn build(&self, state: &WorkState<'_>, trajectory: &WorkTrajectory<'_>) -> EscalationContext;
}

/// The default: everything the loop knows, in the order the loop has always sent it.
#[derive(Debug, Clone, Copy, Default)]
pub struct FullEscalationContext;

impl FullEscalationContext {
    pub const ID: &'static str = "full-v1";
}

impl EscalationContextPolicy for FullEscalationContext {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn build(&self, state: &WorkState<'_>, trajectory: &WorkTrajectory<'_>) -> EscalationContext {
        EscalationContext {
            goal: state.goal.to_string(),
            current_state: format!(
                "turn {} of {}; executions {} of {}",
                state.turn + 1,
                state.max_turns,
                state.executions,
                state.max_executions
            ),
            relevant_evidence: trajectory.evidence.to_vec(),
            relevant_observations: trajectory.observations.to_vec(),
            prior_decisions: trajectory
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
            ruled_out: trajectory.ruled_out.to_vec(),
            omitted: Vec::new(),
            question: trajectory.question.to_string(),
        }
    }
}

/// How an observation relates to the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationClass {
    /// The first observation of its capability.
    New,
    /// The same invocation (capability and inputs) as an earlier observation, with the same result.
    RepeatedIdentical,
    /// The capability was observed before, but with different inputs.
    NewFromSameCapability,
    /// The same invocation as an earlier observation, with a different result: reality changed
    /// between the two, and the later observation is what is now true.
    ChangedReality,
}

/// Where one observation came from, recorded by Chip when it performed the request. Never read
/// from a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationOrigin {
    pub capability: CapabilityId,
    /// The capability and its complete inputs: two observations with the same key answered the same
    /// request.
    pub invocation: String,
    /// The capability's own contract: may an earlier observation of it stand in for a later one?
    /// Only such an observation may ever be left out of a request.
    pub reusable: bool,
    /// The provider's id for the model response that asked for this request, when a model did and
    /// the provider gave one. Correlation metadata: not the execution's identity, not a key.
    pub provider_response_id: Option<String>,
}

/// Two observations say the same thing about reality: same kind, status and output. Their
/// identifiers and receipts differ by construction and say nothing about reality.
fn same_reality(a: &Observation, b: &Observation) -> bool {
    a.kind == b.kind && a.status == b.status && a.output == b.output
}

/// Classifies each observation against the ones before it.
pub fn classify_observations(
    origins: &[ObservationOrigin],
    observations: &[Observation],
) -> Vec<ObservationClass> {
    (0..observations.len().min(origins.len()))
        .map(|i| {
            let earlier = (0..i).rev();
            let same_call = earlier
                .clone()
                .find(|j| origins[*j].invocation == origins[i].invocation);
            match same_call {
                Some(j) if same_reality(&observations[j], &observations[i]) => {
                    ObservationClass::RepeatedIdentical
                }
                Some(_) => ObservationClass::ChangedReality,
                None if earlier
                    .clone()
                    .any(|j| origins[j].capability == origins[i].capability) =>
                {
                    ObservationClass::NewFromSameCapability
                }
                None => ObservationClass::New,
            }
        })
        .collect()
}

/// The observations that may be left out of a request: `(omitted, kept)` index pairs. An
/// observation is left out only when its capability's contract says an earlier observation may
/// stand in for a later one, and a *later observation of the same invocation with the same result*
/// is in the request. The later one was performed and observed; the earlier one adds nothing it does
/// not say. Anything else (a non-reusable capability, or a result that changed) is always kept.
pub fn omissions(
    origins: &[ObservationOrigin],
    observations: &[Observation],
) -> Vec<(usize, usize)> {
    let n = observations.len().min(origins.len());
    (0..n)
        .filter(|i| origins[*i].reusable)
        .filter_map(|i| {
            (i + 1..n)
                .rev()
                .find(|j| {
                    origins[*j].invocation == origins[i].invocation
                        && same_reality(&observations[i], &observations[*j])
                })
                .map(|j| (i, j))
        })
        .collect()
}

/// Like [`FullEscalationContext`], except that an observation a capability's contract allows to be
/// left out, and that a later identical observation makes redundant, is left out and named. Nothing
/// is summarised, truncated or reordered.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeduplicatedEscalationContext;

impl DeduplicatedEscalationContext {
    pub const ID: &'static str = "dedup-v1";
}

impl EscalationContextPolicy for DeduplicatedEscalationContext {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn build(&self, state: &WorkState<'_>, trajectory: &WorkTrajectory<'_>) -> EscalationContext {
        let mut context = FullEscalationContext.build(state, trajectory);
        let left_out = omissions(trajectory.origins, trajectory.observations);
        if left_out.is_empty() {
            return context;
        }
        let id = |i: usize| trajectory.observations[i].execution_id.clone();
        context.omitted = left_out
            .iter()
            .map(|(i, j)| {
                format!(
                    "execution {} of {} is identical to execution {}",
                    id(*i),
                    trajectory.origins[*i].capability,
                    id(*j)
                )
            })
            .collect();
        context.relevant_observations = trajectory
            .observations
            .iter()
            .enumerate()
            .filter(|(i, _)| !left_out.iter().any(|(o, _)| o == i))
            .map(|(_, o)| o.clone())
            .collect();
        context
    }
}

/// One model call's context, as Chip measured it before sending and as the provider reported it
/// after. The two are kept apart: bytes are Chip's own count of message content; tokens are only
/// what a provider said, and `None` when it said nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextCall {
    pub turn: usize,
    /// 1-based, in the order the calls were made.
    pub call: usize,
    pub request_bytes: usize,
    pub messages: usize,
    pub system_messages: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    /// Observations in the request (each is a capability result).
    pub observations: usize,
    /// Observations Chip had recorded when the request was built.
    pub observations_known: usize,
    pub omitted_observations: usize,
    pub reported_prompt_tokens: Option<u32>,
    pub reported_completion_tokens: Option<u32>,
    pub succeeded: bool,
}

/// How much of the observation stream was new, repeated or changed, and what repeating cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObservationRepetition {
    pub new: usize,
    pub repeated_identical: usize,
    pub new_from_same_capability: usize,
    pub changed_reality: usize,
    /// Repeated-identical observations whose earlier copy the capability's contract does not allow
    /// to be left out, so it was sent again.
    pub repeated_and_retained: usize,
    /// Bytes of output in those retained earlier copies, per call they were sent in: what leaving
    /// them out would have saved had the contracts allowed it.
    pub retained_repeat_bytes_sent: usize,
}

/// The whole run's context measurements. Derived from the events and observations only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextReport {
    pub calls: Vec<ContextCall>,
    /// Requests refused because they would have exceeded the budget.
    pub context_limit_rejections: usize,
    pub budget_bytes: Option<usize>,
    pub repetition: ObservationRepetition,
}

impl ContextReport {
    pub fn max_request_bytes(&self) -> usize {
        self.calls
            .iter()
            .map(|c| c.request_bytes)
            .max()
            .unwrap_or(0)
    }
    pub fn total_request_bytes(&self) -> usize {
        self.calls.iter().map(|c| c.request_bytes).sum()
    }
    pub fn max_request_messages(&self) -> usize {
        self.calls.iter().map(|c| c.messages).max().unwrap_or(0)
    }
    pub fn omitted_observations(&self) -> usize {
        self.calls.iter().map(|c| c.omitted_observations).sum()
    }
    pub fn max_reported_input_tokens(&self) -> Option<u32> {
        self.calls
            .iter()
            .filter_map(|c| c.reported_prompt_tokens)
            .max()
    }
    pub fn total_reported_tokens(&self) -> Option<u64> {
        let mut any = false;
        let mut total = 0u64;
        for c in &self.calls {
            if let (Some(p), Some(o)) = (c.reported_prompt_tokens, c.reported_completion_tokens) {
                any = true;
                total += u64::from(p) + u64::from(o);
            }
        }
        any.then_some(total)
    }
}

/// Measures the context of a finished run.
pub fn context_report(report: &WorkReport, spec: &WorkSpec) -> ContextReport {
    let mut calls: Vec<ContextCall> = Vec::new();
    let mut known = 0usize;
    let mut pending: Option<(usize, ContextMetrics, usize)> = None;
    let mut rejections = 0usize;
    for event in &report.events {
        match event {
            WorkEvent::ObservationRecorded { .. } | WorkEvent::EvidenceReused { .. } => known += 1,
            WorkEvent::ContextLimit { .. } => rejections += 1,
            WorkEvent::ModelEscalation { turn, context, .. } => {
                pending = Some((*turn, *context, known));
            }
            WorkEvent::ModelCalled {
                usage, succeeded, ..
            } => {
                if let Some((turn, m, known_then)) = pending.take() {
                    calls.push(ContextCall {
                        turn,
                        call: calls.len() + 1,
                        request_bytes: m.bytes,
                        messages: m.messages,
                        system_messages: m.system_messages,
                        user_messages: m.user_messages,
                        assistant_messages: m.assistant_messages,
                        observations: m.observations,
                        observations_known: known_then,
                        omitted_observations: m.omitted_observations,
                        reported_prompt_tokens: usage.map(|u| u.prompt_tokens),
                        reported_completion_tokens: usage.map(|u| u.completion_tokens),
                        succeeded: *succeeded,
                    });
                }
            }
            _ => {}
        }
    }
    let classes = classify_observations(&report.origins, &report.observations);
    let mut repetition = ObservationRepetition::default();
    for (i, class) in classes.iter().enumerate() {
        match class {
            ObservationClass::New => repetition.new += 1,
            ObservationClass::NewFromSameCapability => repetition.new_from_same_capability += 1,
            ObservationClass::ChangedReality => repetition.changed_reality += 1,
            ObservationClass::RepeatedIdentical => {
                repetition.repeated_identical += 1;
                // The earlier copy this one repeats: sent in every later call that still carried it.
                let earlier = (0..i)
                    .rev()
                    .find(|j| report.origins[*j].invocation == report.origins[i].invocation);
                if let Some(j) = earlier {
                    if !report.origins[j].reusable {
                        repetition.repeated_and_retained += 1;
                        let size = report.observations[j].output.as_deref().map_or(0, str::len);
                        let carried = calls.iter().filter(|c| c.observations_known > i).count();
                        repetition.retained_repeat_bytes_sent += size * carried;
                    }
                }
            }
        }
    }
    ContextReport {
        calls,
        context_limit_rejections: rejections,
        budget_bytes: spec.context_budget_bytes,
        repetition,
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
        /// The identifier of the policy that chose this context. Never prompt text.
        context_policy: String,
        context: ContextMetrics,
    },
    /// The one model call an escalation makes has returned or failed. `usage` is what the
    /// provider reported: `None` when the call failed, and also when it succeeded but reported
    /// no usage (a provider that omits usage yields zero tokens, which no real call can use, so
    /// zero is read as "not reported" and nothing is estimated).
    ModelCalled {
        work_id: WorkId,
        turn: usize,
        usage: Option<ModelUsage>,
        succeeded: bool,
    },
    /// The next model request was not sent: it would have exceeded the configured context budget.
    /// Nothing was truncated, retried or substituted; the work ends at this limit.
    ContextLimit {
        work_id: WorkId,
        turn: usize,
        request_bytes: usize,
        budget_bytes: usize,
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
    /// An authoritative observation was compared with the goal's required output. Only emitted when
    /// the work has one. A receipt and an observation say what happened; this says whether it was
    /// what the goal needed.
    GoalEvaluated {
        work_id: WorkId,
        turn: usize,
        /// This observation produced one of the required outputs.
        satisfied: bool,
        /// Required outputs still unobserved after this evaluation; the work may complete at zero.
        remaining: usize,
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
    /// The policy that chose the escalation context; `None` when nothing escalated.
    pub context_policy: Option<String>,

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
            context_policy: None,
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
                WorkEvent::ModelEscalation {
                    context,
                    context_policy,
                    ..
                } => {
                    m.model_escalations += 1;
                    m.context_policy
                        .get_or_insert_with(|| context_policy.clone());
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
    /// Where each observation came from, in the same order.
    pub origins: Vec<ObservationOrigin>,
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
    /// The outputs authoritative observations must have produced for the goal to be satisfied: one
    /// entry per independently verified outcome. Chip compares each with what Compute observed;
    /// a model's words are never consulted. While any is unmet the work cannot complete. Empty
    /// means the work has no requirement (nothing is evaluated).
    pub required_outputs: Vec<String>,
    /// Requirements an authoritative observation must satisfy, each decided by a predicate the
    /// caller supplies. For an outcome that is not a fixed string: the predicate reads the
    /// observation and nothing else, and says whether it establishes the outcome. Like
    /// `required_outputs`, while any is unmet the work cannot complete.
    pub required_observations: Vec<Arc<dyn ObservationPredicate>>,
    /// Checks the model's proposed answer against the observations when it asks to complete.
    pub required_answers: Vec<Arc<dyn AnswerPredicate>>,
    /// Invariants the safety audit checks every recorded observation against.
    pub observation_invariants: Vec<Arc<dyn ObservationInvariant>>,
    /// Capabilities whose earlier observations must never answer a later request. The audit holds
    /// the trajectory to it: an `EvidenceReused` event for one of these is a violation.
    pub evidence_reuse_prohibited: Vec<CapabilityId>,
    /// The most bytes a model request may carry (see [`ContextMetrics::bytes`]), when the caller has
    /// been told one. `None` means none is known and none is assumed.
    pub context_budget_bytes: Option<usize>,
}

/// Decides whether one authoritative observation establishes a required outcome.
///
/// It is handed an [`Observation`] and nothing else: not a decision, not a model's reply, not a
/// summary. Whoever supplies a predicate owns what it means; the work loop only asks it, and the
/// safety audit and the utility measurement ask it again of the recorded observations.
pub trait ObservationPredicate: fmt::Debug + Send + Sync {
    /// What is required, in words, for diagnostics.
    fn describe(&self) -> String;
    fn satisfied_by(&self, observation: &Observation) -> bool;

    /// Whether the observations so far, in the order they were recorded, establish the outcome.
    /// The default is "some single observation does". An outcome that depends on order (a
    /// verification that must come after a change it verifies) overrides this. It reads
    /// recorded observations and nothing else, and it can become false again when a later
    /// observation supersedes the one that established it.
    fn satisfied_by_trajectory(&self, observations: &[Observation]) -> bool {
        observations.iter().any(|o| self.satisfied_by(o))
    }
}

/// Decides whether an *answer* the model proposes when it asks to complete is acceptable, given the
/// authoritative observations. This is how a read-only goal is completed: the model proposes the
/// answer, and the runtime accepts it only if reality supports it. The answer is never evidence and
/// is never believed; it is only checked against what was observed. A predicate that accepts
/// everything would let the model manufacture completion, so supplying one is the owner's
/// responsibility, and the safety audit asks it again of the recorded outcome.
pub trait AnswerPredicate: fmt::Debug + Send + Sync {
    /// What the answer must satisfy, in words, for diagnostics.
    fn describe(&self) -> String;
    fn accepts(&self, answer: &str, observations: &[Observation]) -> bool;
}

/// An invariant over authoritative observations that must never be violated, whatever the model
/// asked for. The safety audit evaluates each recorded observation against it independently of
/// the capability that produced it (a filesystem capability's observation must not name a path
/// outside its root, however the capability came to produce it).
pub trait ObservationInvariant: fmt::Debug + Send + Sync {
    /// The counter name reported in the audit.
    fn name(&self) -> &'static str;
    /// How many times this observation violates the invariant.
    fn violations(&self, observation: &Observation) -> usize;

    /// How many violations the observations, in the order recorded, contain. The default is the
    /// sum over each observation. An invariant that must read an observation against what came
    /// after it (a listing that a later change legitimately outdates) overrides this.
    fn violations_in_trajectory(&self, observations: &[Observation]) -> usize {
        observations.iter().map(|o| self.violations(o)).sum()
    }
}

impl WorkSpec {
    pub fn new(id: WorkId, goal: WorkGoal) -> Self {
        Self {
            id,
            goal,
            limits: WorkLimits::default(),
            state: None,
            required_outputs: Vec::new(),
            required_observations: Vec::new(),
            required_answers: Vec::new(),
            observation_invariants: Vec::new(),
            evidence_reuse_prohibited: Vec::new(),
            context_budget_bytes: None,
        }
    }

    /// A capability whose evidence the audit requires to never be reused.
    pub fn with_evidence_reuse_prohibited(mut self, capability: CapabilityId) -> Self {
        self.evidence_reuse_prohibited.push(capability);
        self
    }

    /// The configured context budget in bytes. A request over it is never sent.
    pub fn with_context_budget_bytes(mut self, bytes: usize) -> Self {
        self.context_budget_bytes = Some(bytes);
        self
    }

    /// An invariant the audit holds every recorded observation to.
    pub fn with_observation_invariant(mut self, invariant: Arc<dyn ObservationInvariant>) -> Self {
        self.observation_invariants.push(invariant);
        self
    }

    /// Execution success is not goal satisfaction: with a required output, a completed execution
    /// satisfies the goal only if its observed output equals this value (compared trimmed).
    pub fn with_required_output(mut self, output: impl Into<String>) -> Self {
        self.required_outputs.push(output.into());
        self
    }

    /// An outcome decided by a predicate over an authoritative observation.
    pub fn with_required_observation(mut self, predicate: Arc<dyn ObservationPredicate>) -> Self {
        self.required_observations.push(predicate);
        self
    }

    /// A condition the proposed answer must satisfy against the observations before the work may
    /// complete.
    pub fn with_required_answer(mut self, predicate: Arc<dyn AnswerPredicate>) -> Self {
        self.required_answers.push(predicate);
        self
    }

    /// Several independently verified outcomes: completion needs every one observed.
    pub fn with_required_outputs<I, S>(mut self, outputs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.required_outputs
            .extend(outputs.into_iter().map(Into::into));
        self
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
    context_policy: &'a dyn EscalationContextPolicy,
    events: Vec<WorkEvent>,
    observations: Vec<Observation>,
    origins: Vec<ObservationOrigin>,
    decisions: Vec<DecisionRecord>,
    ruled_out: Vec<String>,
    escalations: Vec<ContextMetrics>,
    capabilities: Vec<Capability>,
    summary: WorkSummary,
    latency: WorkLatency,
    /// Which of the spec's required outputs some authoritative observation has produced.
    satisfied: Vec<bool>,
    /// How many execution identities this work has assigned.
    assigned: usize,
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
        self.record_origin(request);
        self.observations.push(observation.clone());
        self.evaluate_goal(turn, request, &observation);
    }

    /// Records where the observation about to be pushed came from.
    fn record_origin(&mut self, request: &CapabilityRequest) {
        self.origins.push(ObservationOrigin {
            capability: request.capability_id.clone(),
            invocation: format!("{}|{:?}", request.capability_id, request.inputs),
            reusable: self.reusable(&request.capability_id),
            provider_response_id: request.provider_response_id.clone(),
        });
    }

    /// Whether completing now would claim a goal some required output of which no authoritative
    /// observation has produced.
    fn completion_refused(&self) -> bool {
        self.satisfied.iter().any(|met| !met)
    }

    /// Whether the answer the model proposed with its completion is unacceptable against the
    /// observations recorded so far.
    fn answer_refused(&self, answer: &str) -> bool {
        self.spec
            .required_answers
            .iter()
            .any(|p| !p.accepts(answer, &self.observations))
    }

    /// What Chip can say about progress without recommending anything: how many required outputs
    /// authoritative observations have produced so far.
    fn progress_line(&self) -> String {
        let met = self.satisfied.iter().filter(|m| **m).count();
        if met == 0 {
            "no authoritative observation satisfies the goal yet".to_string()
        } else {
            format!(
                "{met} of {} required outputs have been observed and verified",
                self.satisfied.len()
            )
        }
    }

    /// Compares one authoritative observation with the spec's required outputs. It reads the
    /// observation and nothing else: not the model's reply, not a decision, not a summary.
    fn evaluate_goal(
        &mut self,
        turn: usize,
        request: &CapabilityRequest,
        observation: &Observation,
    ) {
        if self.spec.required_outputs.is_empty() && self.spec.required_observations.is_empty() {
            return;
        }
        let produced = (observation.kind == ObservationKind::ExecutionCompleted
            && observation.status == crate::ExecutionStatus::Success)
            .then(|| {
                let output = observation.output.as_deref().map(str::trim);
                self.spec
                    .required_outputs
                    .iter()
                    .position(|r| Some(r.trim()) == output)
            })
            .flatten();
        if let Some(i) = produced {
            self.satisfied[i] = true;
        }
        // Each predicate decides for itself, from the observations recorded so far (the latest is
        // the last). A predicate over the trajectory can stop holding when a later observation
        // supersedes the one that established it; an outcome that held and no longer does is unmet.
        let offset = self.spec.required_outputs.len();
        let mut by_predicate = false;
        for (j, predicate) in self.spec.required_observations.iter().enumerate() {
            let held = self.satisfied[offset + j];
            let holds = predicate.satisfied_by_trajectory(&self.observations);
            self.satisfied[offset + j] = holds;
            if holds && (!held || predicate.satisfied_by(observation)) {
                by_predicate = true;
            }
        }
        let produced = produced.is_some() || by_predicate;
        self.events.push(WorkEvent::GoalEvaluated {
            work_id: self.id(),
            turn,
            satisfied: produced,
            remaining: self.satisfied.iter().filter(|m| !**m).count(),
        });
        if !produced && observation.status == crate::ExecutionStatus::Success {
            // It ran, and ran fine; it was just not what the goal needed. A failed execution is
            // already ruled out as failed.
            self.ruled_out.push(format!(
                "capability {}: executed, but its observation did not satisfy the goal",
                invocation_label(request)
            ));
        }
    }

    fn context(&self, turn: usize, evidence: &[String], question: &str) -> EscalationContext {
        self.context_policy.build(
            &WorkState {
                goal: self.spec.goal.as_str(),
                turn,
                max_turns: self.spec.limits.max_turns,
                executions: self.summary.executions,
                max_executions: self.spec.limits.max_executions,
            },
            &WorkTrajectory {
                observations: &self.observations,
                origins: &self.origins,
                decisions: &self.decisions,
                ruled_out: &self.ruled_out,
                evidence,
                question,
            },
        )
    }

    /// Makes the one model call an escalation allows, and interprets its response.
    async fn escalate(
        &mut self,
        turn: usize,
        reason: String,
        evidence: Vec<String>,
        boundary: &dyn WorkDecisionBoundary,
    ) -> Result<WorkDecision, WorkOutcome> {
        let question = boundary.question(&self.capabilities);
        let context = self.context(turn, &evidence, &question);
        let metrics = context.metrics();
        // Fail closed: a request over the configured budget is not sent, trimmed or retried.
        if let Some(budget) = self.spec.context_budget_bytes {
            if metrics.bytes > budget {
                self.events.push(WorkEvent::ContextLimit {
                    work_id: self.id(),
                    turn,
                    request_bytes: metrics.bytes,
                    budget_bytes: budget,
                });
                return Err(WorkOutcome::LimitReached {
                    limit: LimitKind::Context,
                });
            }
        }
        self.summary.model_escalations += 1;
        self.summary.context_bytes += metrics.bytes;
        self.escalations.push(metrics);
        self.events.push(WorkEvent::ModelEscalation {
            work_id: self.id(),
            turn,
            reason,
            context_policy: self.context_policy.id().to_string(),
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
            usage: called.as_ref().ok().and_then(|(_, r)| {
                (r.usage.prompt_tokens > 0 || r.usage.completion_tokens > 0).then_some(ModelUsage {
                    prompt_tokens: r.usage.prompt_tokens,
                    completion_tokens: r.usage.completion_tokens,
                })
            }),
            succeeded: called.is_ok(),
        });
        let (_, response) = called.map_err(|e| WorkOutcome::Failed {
            reason: format!("model escalation failed: {e}"),
        })?;
        let mut decision = boundary
            .interpret(&response, &self.capabilities)
            .map_err(|e| WorkOutcome::Failed {
                reason: format!("the model's response is not a valid decision: {e}"),
            })?;
        // Chip, not the provider, names the execution: assigned here, once, where the request is
        // authorized, and carried unchanged by the execution, its events, its observation and its
        // evidence. Whatever identity the boundary put on a model's request is replaced.
        if let WorkDecision::RequestCapability(request) = &mut decision {
            request.execution_id = self.assign_execution_id();
        }
        self.decisions.push(DecisionRecord {
            turn,
            source: DecisionSource::Model,
            decision: decision.clone(),
        });
        Ok(decision)
    }

    /// The next execution identity of this work: `<work id>-exec-<n>`, counting from 1. It is
    /// derived from nothing a provider or model supplied, so it cannot repeat within the work and
    /// is distinct across works whose ids are. It is not a cryptographic receipt.
    fn assign_execution_id(&mut self) -> ExecutionId {
        self.assigned += 1;
        ExecutionId::new(format!("{}-exec-{}", self.spec.id, self.assigned))
    }

    /// Whether an earlier observation of this capability may answer a request for it again.
    fn reusable(&self, capability: &CapabilityId) -> bool {
        self.capabilities
            .iter()
            .find(|c| &c.descriptor.id == capability)
            .is_none_or(|c| c.descriptor.reuse_evidence)
    }

    fn lookup(&self, request: &CapabilityRequest) -> EvidenceLookup {
        if !self.reusable(&request.capability_id) {
            return EvidenceLookup::NotFound;
        }
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
                invocation_label(request)
            ));
        }
        self.record_origin(request);
        self.observations.push(observation.clone());
        self.evaluate_goal(turn, request, &observation);
        Step::Next
    }
}

/// Names the invocation a note is about. A capability with no inputs is named by its id. One with
/// inputs is named with them, so a note that this invocation failed or fell short is a note about
/// that invocation: another input to the same capability is a different invocation, and nothing
/// said here rules it out.
fn invocation_label(request: &CapabilityRequest) -> String {
    if request.inputs.is_empty() {
        return request.capability_id.to_string();
    }
    let shown: Vec<String> = request
        .inputs
        .iter()
        .map(|(name, value)| match value {
            InputValue::Text(text) => {
                let clipped: String = text.chars().take(40).collect();
                let more = if text.chars().count() > 40 { "..." } else { "" };
                format!("{name}={:?}", format!("{clipped}{more}"))
            }
            InputValue::Integer(n) => format!("{name}={n}"),
            InputValue::Bool(b) => format!("{name}={b}"),
        })
        .collect();
    format!("{} ({})", request.capability_id, shown.join(", "))
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
        self.run_work_with_context_policy(spec, policy, boundary, &FullEscalationContext)
            .await
    }

    /// [`run_work`](Self::run_work) with an explicit policy for what each escalation tells the
    /// model. `run_work` itself uses [`FullEscalationContext`].
    pub async fn run_work_with_context_policy(
        &self,
        spec: &WorkSpec,
        policy: &dyn LocalWorkPolicy,
        boundary: &dyn WorkDecisionBoundary,
        context_policy: &dyn EscalationContextPolicy,
    ) -> WorkReport {
        let started = Mark::now();
        let mut run = Run {
            agent: self,
            spec,
            context_policy,
            events: vec![WorkEvent::WorkStarted {
                work_id: spec.id.clone(),
                goal: spec.goal.as_str().to_string(),
                limits: spec.limits,
            }],
            observations: Vec::new(),
            origins: Vec::new(),
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
            satisfied: vec![false; spec.required_outputs.len() + spec.required_observations.len()],
            assigned: 0,
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
            origins: run.origins,
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
                // A capability whose result depends on state that changes between requests is
                // never answered from evidence: the request is performed.
                Some(WorkDecision::RequestCapability(request))
                    if !run.reusable(&request.capability_id) =>
                {
                    already_checked = true;
                    let decision = WorkDecision::RequestCapability(request);
                    run.decide_locally(turn, &decision);
                    (Some(decision), None)
                }
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
                        // No reasoner is installed (the product configuration): a locally proposed
                        // request without valid evidence is not run on Chip's own say-so. The
                        // model is asked, exactly as for an `Escalate` verdict.
                        Err(AgentError::Reasoning(ReasoningError::Unavailable(_))) => (
                            None,
                            Some((
                                "no valid evidence for the locally proposed request".to_string(),
                                vec![format!(
                                    "{}: no valid evidence (absent or stale)",
                                    request.capability_id
                                )],
                            )),
                        ),
                        Err(e) => {
                            return WorkOutcome::Failed {
                                reason: e.to_string(),
                            };
                        }
                    }
                }
                // A local completion the evidence does not support is refused before it is
                // recorded: the turn is then an ordinary escalation, with the evidence as it is.
                Some(WorkDecision::Complete { summary })
                    if run.completion_refused() || run.answer_refused(&summary) =>
                (
                    None,
                    Some((
                        "the local completion was refused: no authoritative observation satisfies the goal"
                            .to_string(),
                        vec![run.progress_line()],
                    )),
                ),
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
                WorkDecision::Complete { .. } if run.completion_refused() => {
                    // The claim is the model's; the evidence is reality's. Reality wins, and the
                    // work does not continue on the strength of a claim it just refused.
                    return WorkOutcome::Blocked {
                        reason:
                            "completion refused: no authoritative observation satisfies the goal"
                                .to_string(),
                    };
                }
                WorkDecision::Complete { summary } if run.answer_refused(&summary) => {
                    // The goal's observations hold, but the proposed answer is not supported by
                    // them. Same rule: the claim is refused and the work does not go on from it.
                    return WorkOutcome::Blocked {
                        reason: "completion refused: the proposed answer is not supported by the observations"
                            .to_string(),
                    };
                }
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

/// What an independent audit of a finished run found. Every counter must be zero. The audit reads
/// the event stream and the authoritative observations only, never the loop's own counters, so a
/// fault in the loop cannot hide itself from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SafetyAudit {
    /// Executions the runtime never authorised: started without a requested execution, or beyond
    /// the execution limit.
    pub unauthorized_executions: usize,
    /// A completion without a completion decision, or while the loop's own evaluation still had
    /// requirements outstanding.
    pub unauthorized_completions: usize,
    /// A completion although some required output was never produced by an authoritative
    /// observation. Checked against the observations themselves, not against `GoalEvaluated`.
    pub false_completions: usize,
    pub evidence_without_observation: usize,
    pub observation_without_execution: usize,
    /// An execution requested without a preceding request for a declared capability.
    pub execution_without_valid_request: usize,
    /// More decisions than `max_turns` allows.
    pub limit_violations: usize,
    /// Violations of the spec's observation invariants, by invariant name (every declared
    /// invariant is listed, with `0` when it held).
    pub invariant_violations: std::collections::BTreeMap<String, usize>,
    /// An earlier observation answered a request for a capability the spec prohibits reusing.
    pub stale_evidence_reuse: usize,
    /// Events recorded after the work reached a terminal state, or a second terminal state.
    pub events_after_terminal: usize,
    /// Model requests Chip refused to send because they would have exceeded the context budget.
    /// Not a violation: it is the budget doing its job.
    pub context_limit_rejections: usize,
    /// A model request that was sent although it exceeded the context budget.
    pub context_budget_violations: usize,
    /// An observation left out of a request that its capability's contract did not allow to be
    /// left out, or that no later identical observation made redundant.
    pub unjustified_omissions: usize,
    pub details: Vec<String>,
}

impl SafetyAudit {
    /// Violations of one observation invariant (`0` when it held or was not declared).
    pub fn violations_of(&self, name: &str) -> usize {
        self.invariant_violations.get(name).copied().unwrap_or(0)
    }

    pub fn is_clean(&self) -> bool {
        self.unauthorized_executions == 0
            && self.unauthorized_completions == 0
            && self.false_completions == 0
            && self.evidence_without_observation == 0
            && self.observation_without_execution == 0
            && self.execution_without_valid_request == 0
            && self.limit_violations == 0
            && self.context_budget_violations == 0
            && self.unjustified_omissions == 0
            && self.invariant_violations.values().all(|n| *n == 0)
            && self.stale_evidence_reuse == 0
            && self.events_after_terminal == 0
    }

    /// Fails loudly: a violated invariant is not a metric to be reported, it is a failure.
    pub fn assert_clean(&self) {
        assert!(self.is_clean(), "SAFETY INVARIANT VIOLATED: {self:#?}");
    }
}

/// Audits a finished run against the invariants no model judgment may weaken. `declared` is the
/// capability set the run was given.
pub fn audit_safety(
    report: &WorkReport,
    spec: &WorkSpec,
    declared: &[CapabilityId],
) -> SafetyAudit {
    #[derive(Default)]
    struct Turn {
        requested: Option<CapabilityId>,
        execution_requested: bool,
        started: bool,
        observed: bool,
    }
    let mut audit = SafetyAudit::default();
    let mut turn: Option<Turn> = None;
    let (mut executions, mut turns) = (0usize, 0usize);
    let mut last_remaining: Option<usize> = None;
    let mut completion_decided = false;
    let mut terminal_seen = false;
    let mut observations_known = 0usize;
    for event in &report.events {
        if matches!(event, WorkEvent::EvidenceReused { .. }) {
            observations_known += 1;
        }
        if terminal_seen {
            audit.events_after_terminal += 1;
            audit
                .details
                .push("an event was recorded after the work ended".to_string());
        }
        if matches!(
            event,
            WorkEvent::WorkCompleted { .. }
                | WorkEvent::WorkEscalated { .. }
                | WorkEvent::WorkBlocked { .. }
                | WorkEvent::WorkLimitReached { .. }
                | WorkEvent::WorkFailed { .. }
        ) {
            terminal_seen = true;
        }
        match event {
            WorkEvent::EvidenceReused { capability, .. }
                if spec.evidence_reuse_prohibited.contains(capability) =>
            {
                audit.stale_evidence_reuse += 1;
                audit
                    .details
                    .push(format!("evidence for {capability} was reused"));
            }
            WorkEvent::ContextLimit { .. } => audit.context_limit_rejections += 1,
            WorkEvent::ModelEscalation { context, .. } => {
                if spec
                    .context_budget_bytes
                    .is_some_and(|budget| context.bytes > budget)
                {
                    audit.context_budget_violations += 1;
                    audit.details.push(format!(
                        "a request of {} bytes was sent over the budget",
                        context.bytes
                    ));
                }
                // What was left out must be justified by the observations themselves, read again
                // here: only what a contract allowed, and only what a later identical one covers.
                let allowed = omissions(
                    &report.origins[..observations_known.min(report.origins.len())],
                    &report.observations[..observations_known.min(report.observations.len())],
                )
                .len();
                if context.omitted_observations > allowed {
                    audit.unjustified_omissions += context.omitted_observations - allowed;
                    audit
                        .details
                        .push("an observation was left out of a request without cause".to_string());
                }
            }
            WorkEvent::DecisionStarted { .. } => {
                turns += 1;
                turn = Some(Turn::default());
            }
            WorkEvent::DecisionMade { decision, .. } if decision == "complete" => {
                completion_decided = true;
            }
            WorkEvent::CapabilityRequested { capability, .. } => {
                if let Some(t) = &mut turn {
                    t.requested = Some(capability.clone());
                }
            }
            WorkEvent::Execution(ExecutionEvent::ExecutionRequested { intent, .. }) => {
                let valid = turn
                    .as_ref()
                    .and_then(|t| t.requested.as_ref())
                    .is_some_and(|c| c.as_str() == intent && declared.contains(c));
                if !valid {
                    audit.execution_without_valid_request += 1;
                    audit
                        .details
                        .push(format!("execution of '{intent}' without a valid request"));
                }
                if let Some(t) = &mut turn {
                    t.execution_requested = true;
                }
            }
            WorkEvent::Execution(ExecutionEvent::ExecutionStarted { id }) => {
                executions += 1;
                if !turn.as_ref().is_some_and(|t| t.execution_requested) {
                    audit.unauthorized_executions += 1;
                    audit
                        .details
                        .push(format!("execution {id:?} started without being requested"));
                }
                if let Some(t) = &mut turn {
                    t.started = true;
                }
            }
            WorkEvent::ObservationRecorded { execution_id, .. } => {
                observations_known += 1;
                if !turn.as_ref().is_some_and(|t| t.started) {
                    audit.observation_without_execution += 1;
                    audit
                        .details
                        .push(format!("observation {execution_id:?} without an execution"));
                }
                if let Some(t) = &mut turn {
                    t.observed = true;
                }
            }
            WorkEvent::EvidenceRecorded { capability, .. } => {
                if !turn.as_ref().is_some_and(|t| t.observed) {
                    audit.evidence_without_observation += 1;
                    audit
                        .details
                        .push(format!("evidence for {capability} without an observation"));
                }
            }
            WorkEvent::GoalEvaluated { remaining, .. } => last_remaining = Some(*remaining),
            WorkEvent::WorkCompleted { .. } => {
                if !completion_decided {
                    audit.unauthorized_completions += 1;
                    audit
                        .details
                        .push("completed without a completion decision".to_string());
                }
                if let WorkOutcome::Completed { summary } = &report.outcome {
                    for predicate in &spec.required_answers {
                        if !predicate.accepts(summary, &report.observations) {
                            audit.unauthorized_completions += 1;
                            audit.details.push(format!(
                                "completed with an answer that does not satisfy: {}",
                                predicate.describe()
                            ));
                        }
                    }
                }
                if !spec.required_outputs.is_empty() || !spec.required_observations.is_empty() {
                    if last_remaining != Some(0) {
                        audit.unauthorized_completions += 1;
                        audit
                            .details
                            .push("completed with requirements outstanding".to_string());
                    }
                    let produced: Vec<&str> = report
                        .observations
                        .iter()
                        .filter(|o| {
                            o.kind == ObservationKind::ExecutionCompleted
                                && o.status == crate::ExecutionStatus::Success
                        })
                        .filter_map(|o| o.output.as_deref().map(str::trim))
                        .collect();
                    for required in &spec.required_outputs {
                        if !produced.contains(&required.trim()) {
                            audit.false_completions += 1;
                            audit
                                .details
                                .push(format!("completed, but '{required}' was never observed"));
                        }
                    }
                    // A required observation is checked against the recorded observations
                    // themselves, not against what the loop's evaluation said.
                    for predicate in &spec.required_observations {
                        if !predicate.satisfied_by_trajectory(&report.observations) {
                            audit.false_completions += 1;
                            audit.details.push(format!(
                                "completed, but no observation establishes: {}",
                                predicate.describe()
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for invariant in &spec.observation_invariants {
        let n = invariant.violations_in_trajectory(&report.observations);
        audit
            .invariant_violations
            .insert(invariant.name().to_string(), n);
        if n > 0 {
            audit
                .details
                .push(format!("{n} observation(s) violate {}", invariant.name()));
        }
    }
    if executions > spec.limits.max_executions {
        audit.unauthorized_executions += executions - spec.limits.max_executions;
        audit.details.push(format!(
            "{executions} executions exceed the limit of {}",
            spec.limits.max_executions
        ));
    }
    if turns > spec.limits.max_turns {
        audit.limit_violations += turns - spec.limits.max_turns;
        audit.details.push(format!(
            "{turns} turns exceed the limit of {}",
            spec.limits.max_turns
        ));
    }
    audit
}

/// How much verified useful work a run produced, and what it cost. Derived from a finished run and
/// its spec: from the authoritative observations and the event stream, never from a model's words,
/// a capability selection, a receipt, or a counter the loop kept.
///
/// Utility is not safety. [`audit_safety`] does not read this, and this does not read it.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkUtilityMeasurement {
    pub required_outputs: usize,
    /// Required outputs that some authoritative observation actually produced.
    pub verified_outputs: usize,
    /// `verified_outputs / required_outputs`, in `[0, 1]`; `0.0` when nothing was required.
    pub goal_coverage: f64,
    /// The runtime completed the work. Completion needs every required output verified.
    pub completed: bool,
    pub turns: usize,
    pub model_calls: usize,
    pub executions: usize,
    /// The run ended on a decision the runtime refused (an unusable reply, or an invocation
    /// that carried what the capability does not take).
    pub invalid_decisions: usize,
    /// Executions that ran and observed fine but produced none of the required outputs.
    pub wrong_valid_decisions: usize,
    /// Requests answered from existing evidence: a valid selection that added no verified work.
    pub redundant_selections: usize,
    /// Work after the first unsatisfied authoritative observation: turns begun, executions made
    /// and model calls made after it. This is everything that followed, not only the extra cost
    /// of having been wrong; compare with a matched all-correct run for that.
    pub recovery_turns: usize,
    pub recovery_executions: usize,
    pub recovery_model_calls: usize,
    /// `None` when no call after it reported usage.
    pub recovery_tokens: Option<u64>,
    pub total_latency_ms: u64,
    pub model_latency_ms: u64,
    pub compute_latency_ms: u64,
    /// What providers reported. `None` when no call reported any: nothing is estimated.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// Executions that actually started, by the capability the request named.
    pub executions_by_capability: std::collections::BTreeMap<String, usize>,
    /// Observations of executions that failed.
    pub failed_observations: usize,
    /// Failed observations that were followed by a further execution: the runtime let bounded
    /// judgment continue after reality said no. Counts attempts, not successes.
    pub recoveries: usize,
}

impl WorkUtilityMeasurement {
    fn per(&self, denominator: f64) -> Option<f64> {
        (denominator > 0.0).then(|| self.verified_outputs as f64 / denominator)
    }

    pub fn work_per_model_call(&self) -> Option<f64> {
        self.per(self.model_calls as f64)
    }

    pub fn work_per_execution(&self) -> Option<f64> {
        self.per(self.executions as f64)
    }

    /// Only when a provider reported usage.
    pub fn work_per_token(&self) -> Option<f64> {
        self.total_tokens.and_then(|t| self.per(t as f64))
    }

    pub fn work_per_second(&self) -> Option<f64> {
        self.per(self.total_latency_ms as f64 / 1000.0)
    }
}

/// Measures the verified useful work of a finished run against its spec.
pub fn measure_utility(report: &WorkReport, spec: &WorkSpec) -> WorkUtilityMeasurement {
    let required = spec.required_outputs.len() + spec.required_observations.len();
    // Only an authoritative observation of a completed execution can produce a required output.
    let produced: Vec<&str> = report
        .observations
        .iter()
        .filter(|o| {
            o.kind == ObservationKind::ExecutionCompleted
                && o.status == crate::ExecutionStatus::Success
        })
        .filter_map(|o| o.output.as_deref().map(str::trim))
        .collect();
    let verified = spec
        .required_outputs
        .iter()
        .filter(|r| produced.contains(&r.trim()))
        .count()
        + spec
            .required_observations
            .iter()
            .filter(|p| p.satisfied_by_trajectory(&report.observations))
            .count();

    // One walk over the trajectory: what each turn did, and where the first miss was.
    let (mut turns, mut model_calls, mut executions) = (0usize, 0usize, 0usize);
    let (mut wrong_valid, mut redundant) = (0usize, 0usize);
    let mut executed_this_turn = false;
    let mut requested: Option<String> = None;
    let mut by_capability: std::collections::BTreeMap<String, usize> = Default::default();
    let (mut failed_observations, mut recoveries, mut unrecovered) = (0usize, 0usize, 0usize);
    let mut first_miss: Option<usize> = None;
    let (mut input, mut output): (Option<u64>, Option<u64>) = (None, None);
    let add = |slot: &mut Option<u64>, n: u64| *slot = Some(slot.unwrap_or(0) + n);
    for (at, event) in report.events.iter().enumerate() {
        match event {
            WorkEvent::DecisionStarted { .. } => {
                turns += 1;
                executed_this_turn = false;
            }
            WorkEvent::ModelCalled { usage, .. } => {
                model_calls += 1;
                if let Some(u) = usage {
                    add(&mut input, u64::from(u.prompt_tokens));
                    add(&mut output, u64::from(u.completion_tokens));
                }
            }
            WorkEvent::CapabilityRequested { capability, .. } => {
                requested = Some(capability.to_string());
            }
            WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. }) => {
                executions += 1;
                executed_this_turn = true;
                if let Some(capability) = &requested {
                    *by_capability.entry(capability.clone()).or_default() += 1;
                }
                recoveries += unrecovered;
                unrecovered = 0;
            }
            WorkEvent::ObservationRecorded { kind, .. }
                if *kind == ObservationKind::ExecutionFailed =>
            {
                failed_observations += 1;
                unrecovered += 1;
            }
            WorkEvent::EvidenceReused { .. } => redundant += 1,
            WorkEvent::GoalEvaluated {
                satisfied: false, ..
            } if executed_this_turn => {
                wrong_valid += 1;
                first_miss.get_or_insert(at);
            }
            _ => {}
        }
    }

    // What followed the first unsatisfied observation.
    let (mut r_turns, mut r_exec, mut r_calls) = (0usize, 0usize, 0usize);
    let mut r_tokens: Option<u64> = None;
    if let Some(miss) = first_miss {
        for event in &report.events[miss + 1..] {
            match event {
                WorkEvent::DecisionStarted { .. } => r_turns += 1,
                WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. }) => r_exec += 1,
                WorkEvent::ModelCalled { usage, .. } => {
                    r_calls += 1;
                    if let Some(u) = usage {
                        add(
                            &mut r_tokens,
                            u64::from(u.prompt_tokens) + u64::from(u.completion_tokens),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    let invalid = matches!(&report.outcome, WorkOutcome::Failed { reason }
            if reason.contains("not a valid decision"))
        || matches!(&report.outcome, WorkOutcome::Blocked { reason }
            if reason.starts_with("invalid capability input"));
    WorkUtilityMeasurement {
        required_outputs: required,
        verified_outputs: verified,
        goal_coverage: if required == 0 {
            0.0
        } else {
            verified as f64 / required as f64
        },
        completed: matches!(report.outcome, WorkOutcome::Completed { .. }),
        turns,
        model_calls,
        executions,
        invalid_decisions: usize::from(invalid),
        wrong_valid_decisions: wrong_valid,
        redundant_selections: redundant,
        recovery_turns: r_turns,
        recovery_executions: r_exec,
        recovery_model_calls: r_calls,
        recovery_tokens: r_tokens,
        total_latency_ms: report.latency.total.as_millis() as u64,
        model_latency_ms: report.latency.model.as_millis() as u64,
        compute_latency_ms: report.latency.compute.as_millis() as u64,
        input_tokens: input,
        output_tokens: output,
        total_tokens: input.zip(output).map(|(i, o)| i + o),
        executions_by_capability: by_capability,
        failed_observations,
        recoveries,
    }
}
