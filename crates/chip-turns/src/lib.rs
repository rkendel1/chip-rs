//! One Chip turn over a durable session.
//!
//! `run_turn` opens the session for writing (taking its lock, so no other writer can interleave),
//! reconstructs what the session already knows from its committed history, runs the real Chip work
//! loop, and appends the result as a `history_turn_committed` event through the session writer. The
//! model and the tools run exactly once: nothing here retries them, and a persistence failure is
//! reported as such, never as a completed turn.
//!
//! Event shapes are the existing Zig ones (`chip-session`'s `turn` module); nothing is invented.

mod context;
mod record;
mod run;

pub use context::{ContextLimits, compose_goal, render_history};
pub use record::turn_from_report;
pub use run::{PersistedTurn, Persistence, TurnEnv, TurnError, TurnOptions, run_turn};
