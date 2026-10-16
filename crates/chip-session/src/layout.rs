use crate::{Identifier, SessionError};

/// 1 to 255 bytes of `[A-Za-z0-9._-]`, and not `.` or `..`.
pub fn validate_session_id(id: &str) -> Result<(), SessionError> {
    let ok = !id.is_empty()
        && id.len() <= 255
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(SessionError::InvalidSessionId)
    }
}

/// `commit.<generation>.json`: the committed watermark of one log generation.
pub fn watermark_file_name(generation: Identifier) -> String {
    format!("commit.{generation}.json")
}

pub(crate) const EVENTS_FILE: &str = "events.jsonl";
pub(crate) const AUTHORITY_FILE: &str = "authority.json";
pub(crate) const MANIFEST_FILE: &str = "session.json";
pub(crate) const CHECKPOINT_FILE: &str = "checkpoint.json";
pub(crate) const AUTHORITY_MAX_BYTES: u64 = 16 * 1024;
pub(crate) const WATERMARK_MAX_BYTES: u64 = 16 * 1024;

pub(crate) const SESSION_LOCK_FILE: &str = "session.lock";
pub(crate) const COMMIT_LOCK_FILE: &str = "commit.lock";
pub(crate) const PUBLICATION_INTENT_FILE: &str = "commit.pending.json";
pub(crate) const AUTHORITY_INTENT_FILE: &str = "authority.pending.json";
pub(crate) const INTENT_MAX_BYTES: u64 = 32 * 1024;
pub(crate) const LOCK_DEADLINE_MS: u64 = 2000;
