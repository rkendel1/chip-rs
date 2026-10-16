//! Reduces a committed log to the history it describes, the way Zig's `applyEvent` does for the
//! parts a Rust consumer interprets. Anything it does not interpret is refused, never skipped:
//! `state_replacement_*` events (compaction, migration) and turn kinds other than `assistant` and
//! `interrupted` are `Unsupported`, and a malformed payload is an error naming its sequence number.

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::frame::{Frame, Kind};
use crate::turn::{HistoryTurn, HistoryTurnCommitted};
use crate::{Identifier, SessionError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preferences {
    pub model: String,
    pub effort: String,
    pub fast_mode: bool,
}

/// One committed turn with where it sits in the log.
#[derive(Clone, Debug, PartialEq)]
pub struct CommittedTurn {
    pub seq: u64,
    pub event_id: Identifier,
    pub timestamp_ms: i64,
    pub commit: HistoryTurnCommitted,
}

impl CommittedTurn {
    pub fn turn(&self) -> &HistoryTurn {
        &self.commit.turn
    }
}

/// The session as its log defines it.
#[derive(Clone, Debug, PartialEq)]
pub struct History {
    pub id: String,
    pub created_at_ms: i64,
    pub origin_workspace_root: String,
    pub workspace_root: String,
    pub conversation_language: String,
    pub preferences: Preferences,
    pub turns: Vec<CommittedTurn>,
    /// Cumulative, as the last commit recorded them.
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// A model turn started (a recovery checkpoint was written) and neither finished nor was
    /// cleared. Zig resumes such a turn with `--continue-recovery`; a Rust turn must not start over
    /// it, because that would repeat work whose outcome is unknown.
    pub pending_recovery: bool,
    pub last_seq: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreferencesWire {
    model: String,
    effort: String,
    fast_mode: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartedWire {
    id: String,
    created_at_ms: i64,
    origin_workspace_root: String,
    workspace_root: String,
    conversation_language: String,
    preferences: PreferencesWire,
    #[serde(default)]
    #[allow(dead_code)]
    usage: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrefsChangedWire {
    model: Option<String>,
    effort: Option<String>,
    fast_mode: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReboundWire {
    #[allow(dead_code)]
    previous_workspace_root: String,
    workspace_root: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageWire {
    #[allow(dead_code)]
    usage: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointWire {
    #[allow(dead_code)]
    checkpoint: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

fn at(frame: &Frame, error: impl ToString) -> SessionError {
    SessionError::InvalidFormat {
        file: format!("events.jsonl seq {}", frame.seq),
        reason: error.to_string(),
    }
}

fn parse<'a, T: Deserialize<'a>>(frame: &'a Frame) -> Result<T, SessionError> {
    serde_json::from_str(frame.payload.get()).map_err(|e| at(frame, e))
}

/// Reduces committed frames. The first frame must be `session_started`, and it may appear once.
pub fn reduce_history(frames: &[Frame]) -> Result<History, SessionError> {
    let mut iter = frames.iter();
    let first = iter
        .next()
        .ok_or_else(|| SessionError::WatermarkMismatch("the log has no events".into()))?;
    if first.kind != Kind::SessionStarted {
        return Err(at(first, "the first event is not session_started"));
    }
    let started: StartedWire = parse(first)?;
    let mut history = History {
        id: started.id,
        created_at_ms: started.created_at_ms,
        origin_workspace_root: started.origin_workspace_root,
        workspace_root: started.workspace_root,
        conversation_language: started.conversation_language,
        preferences: Preferences {
            model: started.preferences.model,
            effort: started.preferences.effort,
            fast_mode: started.preferences.fast_mode,
        },
        turns: Vec::new(),
        total_input_tokens: 0,
        total_output_tokens: 0,
        pending_recovery: false,
        last_seq: first.seq,
    };
    for frame in iter {
        match frame.kind {
            Kind::SessionStarted => {
                return Err(at(frame, "session_started appears more than once"));
            }
            Kind::PreferencesChanged => {
                let changed: PrefsChangedWire = parse(frame)?;
                if changed.model.is_none()
                    && changed.effort.is_none()
                    && changed.fast_mode.is_none()
                {
                    return Err(at(frame, "preferences_changed changes nothing"));
                }
                if let Some(model) = changed.model {
                    history.preferences.model = model;
                }
                if let Some(effort) = changed.effort {
                    history.preferences.effort = effort;
                }
                if let Some(fast) = changed.fast_mode {
                    history.preferences.fast_mode = fast;
                }
            }
            Kind::WorkspaceRebound => {
                history.workspace_root = parse::<ReboundWire>(frame)?.workspace_root
            }
            Kind::UsageCheckpointed => {
                parse::<UsageWire>(frame)?;
            }
            Kind::RecoveryCheckpointSet => {
                parse::<CheckpointWire>(frame)?;
                history.pending_recovery = true;
            }
            Kind::RecoveryCheckpointCleared => {
                parse::<Empty>(frame)?;
                history.pending_recovery = false;
            }
            Kind::HistoryTurnCommitted => {
                let commit =
                    HistoryTurnCommitted::from_payload_json(frame.payload.get()).map_err(|e| {
                        match e {
                            SessionError::InvalidFormat { reason, .. } => at(frame, reason),
                            other => other,
                        }
                    })?;
                history.conversation_language = commit.conversation_language.clone();
                history.total_input_tokens = commit.total_input_tokens;
                history.total_output_tokens = commit.total_output_tokens;
                // A commit closes the active turn, as it does in Zig's reduction.
                history.pending_recovery = false;
                history.turns.push(CommittedTurn {
                    seq: frame.seq,
                    event_id: frame.event_id,
                    timestamp_ms: frame.timestamp_ms,
                    commit,
                });
            }
            Kind::StateReplacementStarted
            | Kind::StateReplacementChunk
            | Kind::StateReplacementCommitted => {
                return Err(SessionError::Unsupported(format!(
                    "{:?} at seq {} (compaction and migration are not interpreted)",
                    frame.kind, frame.seq
                )));
            }
        }
        history.last_seq = frame.seq;
    }
    Ok(history)
}
