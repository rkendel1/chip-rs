use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::{Identifier, SessionError};

/// A frame, including its newline, may not exceed 8 MiB.
pub const EVENT_FRAME_MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    SessionStarted,
    PreferencesChanged,
    WorkspaceRebound,
    HistoryTurnCommitted,
    UsageCheckpointed,
    RecoveryCheckpointSet,
    RecoveryCheckpointCleared,
    StateReplacementStarted,
    StateReplacementChunk,
    StateReplacementCommitted,
}

/// One decoded event. The payload is kept as the exact JSON the frame carried; its meaning
/// (turns, usage, checkpoints) belongs to the layer above.
#[derive(Debug)]
pub struct Frame {
    pub log_generation: Identifier,
    pub seq: u64,
    pub event_id: Identifier,
    pub timestamp_ms: i64,
    pub kind: Kind,
    pub payload: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFrame {
    schema_version: u64,
    log_generation: String,
    seq: u64,
    event_id: String,
    timestamp_ms: i64,
    kind: Kind,
    payload: Box<RawValue>,
}

#[derive(Serialize)]
struct WireFrameOut<'a> {
    schema_version: u64,
    log_generation: String,
    seq: u64,
    event_id: String,
    timestamp_ms: i64,
    kind: Kind,
    payload: &'a RawValue,
}

/// Decodes one line (with its trailing newline). Exactly the seven keys, each once; schema 1;
/// a known kind; 32-hex identifiers.
pub fn decode_frame(line: &[u8], offset: u64) -> Result<Frame, SessionError> {
    let invalid = |reason: &str| SessionError::InvalidFrame {
        offset,
        reason: reason.into(),
    };
    if line.len() > EVENT_FRAME_MAX_BYTES {
        return Err(SessionError::EventFrameTooLarge { offset });
    }
    let body = match line.split_last() {
        Some((b'\n', body)) if !body.contains(&b'\n') => body,
        _ => return Err(invalid("a frame is exactly one line ending in a newline")),
    };
    let wire: WireFrame = serde_json::from_slice(body).map_err(|e| invalid(&e.to_string()))?;
    if wire.schema_version != 1 {
        return Err(SessionError::UnsupportedSchema {
            offset,
            found: wire.schema_version,
        });
    }
    Ok(Frame {
        log_generation: Identifier::parse(&wire.log_generation)
            .ok_or_else(|| invalid("log_generation is not 32 lowercase hex"))?,
        seq: wire.seq,
        event_id: Identifier::parse(&wire.event_id)
            .ok_or_else(|| invalid("event_id is not 32 lowercase hex"))?,
        timestamp_ms: wire.timestamp_ms,
        kind: wire.kind,
        payload: wire.payload,
    })
}

/// Encodes a frame in the order and spelling the Zig implementation writes.
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, SessionError> {
    let out = WireFrameOut {
        schema_version: 1,
        log_generation: frame.log_generation.to_string(),
        seq: frame.seq,
        event_id: frame.event_id.to_string(),
        timestamp_ms: frame.timestamp_ms,
        kind: frame.kind,
        payload: &frame.payload,
    };
    let mut bytes = serde_json::to_vec(&out).map_err(|e| SessionError::InvalidFrame {
        offset: 0,
        reason: e.to_string(),
    })?;
    bytes.push(b'\n');
    if bytes.len() > EVENT_FRAME_MAX_BYTES {
        return Err(SessionError::EventFrameTooLarge { offset: 0 });
    }
    Ok(bytes)
}
