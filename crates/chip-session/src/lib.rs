//! Reader and writer for the durable session store of the Zig `fx` (`event_log_v1`).
//!
//! The session directory is owned by the format the Zig implementation defines. This crate does
//! not invent a second source of truth: it reads the same files with the same strictness
//! (exact key sets, a committed watermark, contiguous sequence numbers) and reports what is
//! committed. Bytes after the committed watermark are an uncommitted tail and are never
//! returned as history.
//!
//! Format reference: `docs/product/session-format.md`.

mod error;
mod frame;
mod history;
mod id;
mod layout;
mod store;
mod turn;
mod writer;

pub use error::SessionError;
pub use frame::{EVENT_FRAME_MAX_BYTES, Frame, Kind, decode_frame, encode_frame};
pub use history::{CommittedTurn, History, Preferences, reduce_history};
pub use id::{IdSource, Identifier, OsIds};
pub use layout::{validate_session_id, watermark_file_name};
pub use store::{
    Authority, AuthoritySource, CheckpointStatus, CommitPosition, CommittedLog, ManifestStatus,
    read_session,
};
pub use turn::{
    EXECUTION_SCHEMA, ExecutionMemory, FileAction, FileEvidence, HistoryTurn, HistoryTurnCommitted,
    ImageAttachment, RawJson, TerminalReason, ToolCall, ToolResult, ToolStatus, ToolStep, UserTurn,
};
pub use writer::{Boundary, NewEvent, SessionWriter, encode_authority, encode_watermark};
