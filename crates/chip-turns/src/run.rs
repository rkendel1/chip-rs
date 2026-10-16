use std::path::Path;

use chip_core::{
    Agent, LocalWorkPolicy, WorkDecisionBoundary, WorkGoal, WorkId, WorkReport, WorkSpec,
};
use chip_session::{
    Boundary, CommitPosition, IdSource, Kind, NewEvent, SessionError, SessionWriter, read_session,
    reduce_history,
};

use crate::context::{ContextLimits, compose_goal};
use crate::record::turn_from_report;

/// What happened to the turn's record. The work itself ran exactly once either way.
#[derive(Debug)]
pub enum Persistence {
    /// The turn is durable: the watermark names it.
    Committed(CommitPosition),
    /// Nothing was published (the tail was rolled back). The turn is not in the session.
    NotCommitted(String),
    /// The write may or may not have been published. The next open of the session resolves it; this
    /// turn must not be assumed either way.
    Uncertain(String),
}

#[derive(Debug)]
pub struct PersistedTurn {
    pub report: WorkReport,
    pub persistence: Persistence,
}

/// Why no turn ran at all. In each case the model and the tools were not called.
#[derive(Debug)]
pub enum TurnError {
    /// The session could not be opened, was held by another writer, or its history is not one this
    /// implementation interprets.
    Session(SessionError),
    /// A model turn was started in the session and never finished or cleared (its recovery
    /// checkpoint is still pending). Starting a new turn over it would repeat work whose outcome is
    /// unknown; resume it with the implementation that wrote it.
    PendingRecovery,
    /// Test-only: a configured crash point stopped the writer as if the process had died.
    SimulatedCrash(&'static str),
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session(error) => write!(f, "{error}"),
            Self::PendingRecovery => write!(
                f,
                "the session has an unfinished turn (pending recovery); not starting another over it"
            ),
            Self::SimulatedCrash(at) => write!(f, "simulated crash at {at}"),
        }
    }
}

impl std::error::Error for TurnError {}

impl From<SessionError> for TurnError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

pub struct TurnOptions {
    pub context: ContextLimits,
    /// Wall-clock milliseconds, injected so recorded timestamps are reproducible in tests.
    pub now_ms: i64,
    /// Test-only crash injection for the writer.
    pub crash_at: Option<Boundary>,
}

impl Default for TurnOptions {
    fn default() -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self {
            context: ContextLimits::default(),
            now_ms,
            crash_at: None,
        }
    }
}

/// What runs the turn: the agent, and the policy and boundary that govern its decisions.
pub struct TurnEnv<'a> {
    pub agent: &'a Agent,
    pub policy: &'a dyn LocalWorkPolicy,
    pub boundary: &'a dyn WorkDecisionBoundary,
}

/// Runs one turn over `session_dir` and persists it.
///
/// `build_spec` receives the goal (earlier history plus the prompt) and returns the work spec, so
/// the caller keeps control of limits and required outcomes. The session stays locked for the whole
/// turn.
pub async fn run_turn(
    session_dir: &Path,
    env: TurnEnv<'_>,
    prompt: &str,
    build_spec: impl FnOnce(WorkId, WorkGoal) -> WorkSpec,
    ids: Box<dyn IdSource>,
    options: TurnOptions,
) -> Result<PersistedTurn, TurnError> {
    let mut writer = SessionWriter::open(session_dir, ids, options.crash_at)?;
    let committed = read_session(session_dir)?;
    let history = reduce_history(&committed.frames)?;
    if history.pending_recovery {
        return Err(TurnError::PendingRecovery);
    }
    let goal = compose_goal(&history, prompt, options.context);
    let spec = build_spec(
        WorkId::new(format!("turn-{}", history.last_seq + 1)),
        WorkGoal::new(goal),
    );
    let report = env.agent.run_work(&spec, env.policy, env.boundary).await;

    let commit = turn_from_report(&history, prompt, &report, options.now_ms)?;
    let payload = commit.to_payload_json()?;
    let event = NewEvent::new(Kind::HistoryTurnCommitted, options.now_ms, &payload)?;
    let persistence = match writer.append(vec![event]) {
        Ok(position) => Persistence::Committed(position),
        Err(SessionError::PersistenceDegraded(why)) => Persistence::NotCommitted(why),
        Err(SessionError::CommitIndeterminate(why)) => Persistence::Uncertain(why),
        Err(SessionError::SimulatedCrash(at)) => return Err(TurnError::SimulatedCrash(at)),
        Err(other) => Persistence::Uncertain(format!("persisting the turn failed: {other}")),
    };
    Ok(PersistedTurn {
        report,
        persistence,
    })
}
