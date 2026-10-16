use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::frame::{Frame, decode_frame};
use crate::layout::{
    AUTHORITY_FILE, AUTHORITY_MAX_BYTES, CHECKPOINT_FILE, EVENTS_FILE, MANIFEST_FILE,
    WATERMARK_MAX_BYTES, validate_session_id, watermark_file_name,
};
use crate::{EVENT_FRAME_MAX_BYTES, Identifier, SessionError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthoritySource {
    NativeCreate,
    LegacyMigration,
}

/// `authority.json`: which implementation created the session and under which storage format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authority {
    pub session_id: String,
    pub authority_id: Identifier,
    pub source: AuthoritySource,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityWire {
    schema_version: u64,
    session_id: String,
    authority_id: String,
    storage_format: String,
    source: AuthoritySource,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WatermarkWire {
    schema_version: u64,
    session_id: String,
    log_generation: String,
    through_seq: u64,
    through_event_id: String,
    through_event_log_bytes: u64,
}

/// The last event that is durably committed, and where it ends in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitPosition {
    pub log_generation: Identifier,
    pub through_seq: u64,
    pub through_event_id: Identifier,
    pub through_event_log_bytes: u64,
}

/// The manifest (`session.json`) is a projection of the log. A stale or missing one is not an
/// error here; the log and its watermark are the truth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestStatus {
    Current,
    Stale { reason: String },
    Missing,
    Invalid,
}

/// `checkpoint.json` is also a projection; the manifest records its SHA-256.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointStatus {
    Verified,
    DigestMismatch,
    Missing,
    NotRecorded,
}

/// What `read_session` proved is committed.
#[derive(Debug)]
pub struct CommittedLog {
    pub session_id: String,
    pub authority: Authority,
    pub position: CommitPosition,
    /// Every committed frame in order. The first frame's `seq` is the generation base.
    pub frames: Vec<Frame>,
    /// Bytes after the committed watermark: an unfinished append that is not history.
    pub uncommitted_tail_bytes: u64,
    pub manifest: ManifestStatus,
    pub checkpoint: CheckpointStatus,
}

pub(crate) fn open_regular(dir: &Path, name: &str) -> Result<File, SessionError> {
    let path = dir.join(name);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SessionError::Missing(name.to_string()));
        }
        Err(e) => return Err(e.into()),
    };
    if !meta.file_type().is_file() {
        return Err(SessionError::UnsafePath(format!(
            "{name} is not a regular file"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Err(SessionError::UnsafePath(format!(
                "{name} has {} hard links",
                meta.nlink()
            )));
        }
    }
    Ok(File::open(path)?)
}

pub(crate) fn read_bounded(dir: &Path, name: &str, max: u64) -> Result<Vec<u8>, SessionError> {
    let file = open_regular(dir, name)?;
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() as u64 > max {
        return Err(SessionError::InvalidFormat {
            file: name.into(),
            reason: format!("empty or over {max} bytes"),
        });
    }
    Ok(bytes)
}

fn invalid(file: &str, reason: impl ToString) -> SessionError {
    SessionError::InvalidFormat {
        file: file.into(),
        reason: reason.to_string(),
    }
}

pub(crate) fn read_authority(dir: &Path, session_id: &str) -> Result<Authority, SessionError> {
    let bytes = read_bounded(dir, AUTHORITY_FILE, AUTHORITY_MAX_BYTES)?;
    let wire: AuthorityWire =
        serde_json::from_slice(&bytes).map_err(|e| invalid(AUTHORITY_FILE, e))?;
    if wire.schema_version != 1 || wire.storage_format != "event_log_v1" {
        return Err(invalid(AUTHORITY_FILE, "not schema 1 / event_log_v1"));
    }
    validate_session_id(&wire.session_id)?;
    if wire.session_id != session_id {
        return Err(invalid(
            AUTHORITY_FILE,
            "session_id does not match the directory",
        ));
    }
    Ok(Authority {
        session_id: wire.session_id,
        authority_id: Identifier::parse(&wire.authority_id)
            .ok_or_else(|| invalid(AUTHORITY_FILE, "authority_id is not 32 lowercase hex"))?,
        source: wire.source,
    })
}

/// Reads the first line of the log to learn the live generation.
pub(crate) fn first_generation(log: &mut File) -> Result<Identifier, SessionError> {
    let mut line = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = log.read(&mut buf)?;
        if n == 0 {
            return Err(SessionError::TruncatedFrame { offset: 0 });
        }
        if let Some(pos) = buf[..n].iter().position(|&b| b == b'\n') {
            line.extend_from_slice(&buf[..=pos]);
            break;
        }
        line.extend_from_slice(&buf[..n]);
        if line.len() > EVENT_FRAME_MAX_BYTES {
            return Err(SessionError::EventFrameTooLarge { offset: 0 });
        }
    }
    Ok(decode_frame(&line, 0)?.log_generation)
}

pub(crate) fn read_watermark(
    dir: &Path,
    session_id: &str,
    generation: Identifier,
) -> Result<CommitPosition, SessionError> {
    let name = watermark_file_name(generation);
    let name = name.as_str();
    let bytes = read_bounded(dir, name, WATERMARK_MAX_BYTES)?;
    let wire: WatermarkWire = serde_json::from_slice(&bytes).map_err(|e| invalid(name, e))?;
    if wire.schema_version != 1 {
        return Err(invalid(name, "schema_version is not 1"));
    }
    validate_session_id(&wire.session_id)?;
    let parsed_generation = Identifier::parse(&wire.log_generation)
        .ok_or_else(|| invalid(name, "log_generation is not 32 lowercase hex"))?;
    if wire.session_id != session_id || parsed_generation != generation {
        return Err(SessionError::WatermarkMismatch(
            "session id or generation differs from the log".into(),
        ));
    }
    Ok(CommitPosition {
        log_generation: generation,
        through_seq: wire.through_seq,
        through_event_id: Identifier::parse(&wire.through_event_id)
            .ok_or_else(|| invalid(name, "through_event_id is not 32 lowercase hex"))?,
        through_event_log_bytes: wire.through_event_log_bytes,
    })
}

/// Reads every frame up to the committed watermark and proves the log is intact to that point:
/// each line is a valid frame of the live generation, sequence numbers are contiguous, and the
/// last frame is exactly the one the watermark names, ending exactly at its byte count.
fn read_committed_frames(
    dir: &Path,
    position: CommitPosition,
) -> Result<(Vec<Frame>, u64), SessionError> {
    let mut file = open_regular(dir, EVENTS_FILE)?;
    let length = file.metadata()?.len();
    if length < position.through_event_log_bytes {
        return Err(SessionError::WatermarkMismatch(format!(
            "watermark covers {} bytes but the log has {length}",
            position.through_event_log_bytes
        )));
    }
    let mut committed = vec![0u8; position.through_event_log_bytes as usize];
    file.read_exact(&mut committed)?;

    let mut frames: Vec<Frame> = Vec::new();
    let mut offset = 0u64;
    while (offset as usize) < committed.len() {
        let rest = &committed[offset as usize..];
        let end = match rest.iter().position(|&b| b == b'\n') {
            Some(newline) => newline + 1,
            None => return Err(SessionError::TruncatedFrame { offset }),
        };
        let frame = decode_frame(&rest[..end], offset)?;
        if frame.log_generation != position.log_generation {
            return Err(SessionError::GenerationMismatch { offset });
        }
        if let Some(previous) = frames.last() {
            let expected =
                previous
                    .seq
                    .checked_add(1)
                    .ok_or_else(|| SessionError::InvalidFrame {
                        offset,
                        reason: "seq overflow".into(),
                    })?;
            if frame.seq != expected {
                return Err(SessionError::SequenceGap {
                    offset,
                    expected,
                    found: frame.seq,
                });
            }
        }
        frames.push(frame);
        offset += end as u64;
    }
    let last = frames
        .last()
        .ok_or_else(|| SessionError::WatermarkMismatch("the watermark covers no events".into()))?;
    if last.seq != position.through_seq || last.event_id != position.through_event_id {
        return Err(SessionError::WatermarkMismatch(
            "the last committed frame is not the one the watermark names".into(),
        ));
    }
    Ok((frames, length - position.through_event_log_bytes))
}

#[derive(Deserialize)]
struct ManifestView {
    id: Option<String>,
    log_generation: Option<String>,
    last_event_seq: Option<u64>,
    event_log_bytes: Option<u64>,
    checkpoint_sha256: Option<String>,
}

fn inspect_manifest(
    dir: &Path,
    session_id: &str,
    position: CommitPosition,
) -> (ManifestStatus, CheckpointStatus) {
    let bytes = match read_bounded(dir, MANIFEST_FILE, 1024 * 1024) {
        Ok(bytes) => bytes,
        Err(SessionError::Missing(_)) => {
            return (ManifestStatus::Missing, CheckpointStatus::NotRecorded);
        }
        Err(_) => return (ManifestStatus::Invalid, CheckpointStatus::NotRecorded),
    };
    let Ok(view) = serde_json::from_slice::<ManifestView>(&bytes) else {
        return (ManifestStatus::Invalid, CheckpointStatus::NotRecorded);
    };
    let manifest = if view.id.as_deref() != Some(session_id) {
        ManifestStatus::Stale {
            reason: "id differs from the directory".into(),
        }
    } else if view.log_generation.as_deref().and_then(Identifier::parse)
        != Some(position.log_generation)
    {
        ManifestStatus::Stale {
            reason: "log_generation differs from the log".into(),
        }
    } else if view.last_event_seq != Some(position.through_seq)
        || view.event_log_bytes != Some(position.through_event_log_bytes)
    {
        ManifestStatus::Stale {
            reason: "records an older commit position".into(),
        }
    } else {
        ManifestStatus::Current
    };
    let checkpoint = match view.checkpoint_sha256 {
        None => CheckpointStatus::NotRecorded,
        Some(expected) => match read_bounded(dir, CHECKPOINT_FILE, 64 * 1024 * 1024) {
            Err(SessionError::Missing(_)) => CheckpointStatus::Missing,
            Err(_) => CheckpointStatus::DigestMismatch,
            Ok(bytes) => {
                let digest: String = Sha256::digest(&bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                if digest == expected {
                    CheckpointStatus::Verified
                } else {
                    CheckpointStatus::DigestMismatch
                }
            }
        },
    };
    (manifest, checkpoint)
}

/// Reads one session directory (`<sessions>/<id>`), read-only. Nothing is written, repaired or
/// deleted; an uncommitted tail is reported, never applied.
pub fn read_session(session_dir: &Path) -> Result<CommittedLog, SessionError> {
    let session_id = session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SessionError::InvalidSessionId)?
        .to_string();
    validate_session_id(&session_id)?;
    let meta = fs::symlink_metadata(session_dir)?;
    if !meta.file_type().is_dir() {
        return Err(SessionError::UnsafePath(
            "the session path is not a directory".into(),
        ));
    }
    let authority = read_authority(session_dir, &session_id)?;
    let mut log = open_regular(session_dir, EVENTS_FILE)?;
    let generation = first_generation(&mut log)?;
    drop(log);
    let position = read_watermark(session_dir, &session_id, generation)?;
    let (frames, tail) = read_committed_frames(session_dir, position)?;
    let (manifest, checkpoint) = inspect_manifest(session_dir, &session_id, position);
    Ok(CommittedLog {
        session_id,
        authority,
        position,
        frames,
        uncommitted_tail_bytes: tail,
        manifest,
        checkpoint,
    })
}

/// The live generation of a session directory: the first frame's `log_generation`.
pub(crate) fn live_generation(dir: &Path) -> Result<Identifier, SessionError> {
    let mut log = open_regular(dir, EVENTS_FILE)?;
    first_generation(&mut log)
}

/// Proves the log is intact up to `position` (frames, generation, sequence, last frame).
pub(crate) fn validate_position(dir: &Path, position: CommitPosition) -> Result<(), SessionError> {
    read_committed_frames(dir, position).map(|_| ())
}
