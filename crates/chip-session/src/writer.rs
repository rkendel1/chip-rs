//! Writes the Zig-owned `event_log_v1` store with the same commit protocol: append and sync the
//! frames, take the commit lock, durably record a publication intent, durably replace the commit
//! watermark, then remove the intent. A crash at any boundary leaves a state the next open resolves
//! to the prior or the proposed position, exactly as the Zig implementation does.
//!
//! Only the truth files are written (`events.jsonl`, the watermark, `authority.json`). The
//! projections (`session.json`, `checkpoint.json`, `display.json`, `usage-v2.json`) are rebuilt by
//! whoever resumes the session; the Zig binary does so on resume (verified in
//! `docs/rebuild/tools/verify_rust_session_resume.py` in rkendel1/fx).

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::frame::{Frame, Kind, encode_frame};
use crate::layout::{
    AUTHORITY_FILE, AUTHORITY_INTENT_FILE, COMMIT_LOCK_FILE, EVENTS_FILE, INTENT_MAX_BYTES,
    LOCK_DEADLINE_MS, PUBLICATION_INTENT_FILE, SESSION_LOCK_FILE, validate_session_id,
    watermark_file_name,
};
use crate::store::{
    CommitPosition, live_generation, open_regular, read_authority, read_bounded, read_watermark,
    validate_position,
};
use crate::{IdSource, Identifier, SessionError};

/// Points where a configured crash stops the writer as if the process had died there. For tests of
/// interrupted writes; production code never sets one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    AfterEventAppend,
    AfterEventSync,
    AfterIntentSync,
    AfterWatermarkRename,
    AfterIntentRemove,
    AfterAuthorityIntentSync,
    AfterAuthorityRename,
}

impl Boundary {
    fn name(self) -> &'static str {
        match self {
            Self::AfterEventAppend => "after_event_append",
            Self::AfterEventSync => "after_event_sync",
            Self::AfterIntentSync => "after_intent_sync",
            Self::AfterWatermarkRename => "after_watermark_rename",
            Self::AfterIntentRemove => "after_intent_remove",
            Self::AfterAuthorityIntentSync => "after_authority_intent_sync",
            Self::AfterAuthorityRename => "after_authority_rename",
        }
    }
}

/// An event to append. The payload is the exact JSON of the frame's `payload`.
pub struct NewEvent {
    pub kind: Kind,
    pub timestamp_ms: i64,
    pub payload: Box<RawValue>,
}

impl NewEvent {
    pub fn new(kind: Kind, timestamp_ms: i64, payload_json: &str) -> Result<Self, SessionError> {
        let payload = RawValue::from_string(payload_json.to_string()).map_err(|e| {
            SessionError::InvalidFrame {
                offset: 0,
                reason: e.to_string(),
            }
        })?;
        Ok(Self {
            kind,
            timestamp_ms,
            payload,
        })
    }
}

pub struct SessionWriter {
    dir: PathBuf,
    session_id: String,
    position: CommitPosition,
    log: File,
    ids: Box<dyn IdSource>,
    crash_at: Option<Boundary>,
    _session_lock: File,
}

fn private_dir_check(path: &Path) -> Result<(), SessionError> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_dir() {
        return Err(SessionError::UnsafePath(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    if meta.permissions().mode() & 0o777 != 0o700 {
        return Err(SessionError::PrivateStatePermissions(format!(
            "{} is not mode 0700",
            path.display()
        )));
    }
    Ok(())
}

fn private_file_check(dir: &Path, name: &str) -> Result<(), SessionError> {
    let meta = match fs::symlink_metadata(dir.join(name)) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SessionError::Missing(name.to_string()));
        }
        Err(e) => return Err(e.into()),
    };
    if !meta.file_type().is_file() || meta.nlink() != 1 {
        return Err(SessionError::UnsafePath(format!(
            "{name} is not a single regular file"
        )));
    }
    if meta.permissions().mode() & 0o777 != 0o600 {
        return Err(SessionError::PrivateStatePermissions(format!(
            "{name} is not mode 0600"
        )));
    }
    Ok(())
}

fn sync_dir(dir: &Path) -> Result<(), SessionError> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn acquire_lock(dir: &Path, name: &str) -> Result<File, SessionError> {
    let path = dir.join(name);
    if let Ok(meta) = fs::symlink_metadata(&path)
        && !meta.file_type().is_file()
    {
        return Err(SessionError::UnsafePath(format!(
            "{name} is not a regular file"
        )));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {
                if started.elapsed() >= Duration::from_millis(LOCK_DEADLINE_MS) {
                    return Err(SessionError::Locked);
                }
                sleep(Duration::from_millis(10));
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

/// Temp file, sync, rename over the target, sync the directory. The target is never partly written.
fn durable_replace(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    ids: &mut dyn IdSource,
) -> Result<(), SessionError> {
    let target = dir.join(name);
    if let Ok(meta) = fs::symlink_metadata(&target)
        && !meta.file_type().is_file()
    {
        return Err(SessionError::UnsafePath(format!(
            "{name} is not a regular file"
        )));
    }
    let temp = dir.join(format!(".{name}.tmp.{}", ids.next_id()?));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| -> Result<(), SessionError> {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
        return result;
    }
    sync_dir(dir)
}

fn delete_and_sync(dir: &Path, name: &str) -> Result<(), SessionError> {
    match fs::remove_file(dir.join(name)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    sync_dir(dir)
}

fn position_json(position: &CommitPosition) -> String {
    format!(
        "{{\"log_generation\":\"{}\",\"through_seq\":{},\"through_event_id\":\"{}\",\"through_event_log_bytes\":{}}}",
        position.log_generation,
        position.through_seq,
        position.through_event_id,
        position.through_event_log_bytes
    )
}

/// The exact bytes of `commit.<generation>.json` for a position, as the Zig implementation writes them.
pub fn encode_watermark(session_id: &str, position: &CommitPosition) -> Vec<u8> {
    format!(
        "{{\"schema_version\":1,\"session_id\":\"{session_id}\",\"log_generation\":\"{}\",\"through_seq\":{},\"through_event_id\":\"{}\",\"through_event_log_bytes\":{}}}\n",
        position.log_generation, position.through_seq, position.through_event_id, position.through_event_log_bytes
    )
    .into_bytes()
}

fn publication_intent_bytes(
    session_id: &str,
    operation: Identifier,
    prior: &CommitPosition,
    proposed: &CommitPosition,
) -> Vec<u8> {
    format!(
        "{{\"schema_version\":1,\"session_id\":\"{session_id}\",\"operation_id\":\"{operation}\",\"kind\":\"watermark_advance\",\"prior\":{},\"proposed\":{}}}\n",
        position_json(prior),
        position_json(proposed)
    )
    .into_bytes()
}

/// The exact bytes of a native `authority.json`.
pub fn encode_authority(session_id: &str, authority_id: Identifier) -> Vec<u8> {
    format!(
        "{{\"schema_version\":1,\"session_id\":\"{session_id}\",\"authority_id\":\"{authority_id}\",\"storage_format\":\"event_log_v1\",\"source\":\"native_create\"}}\n"
    )
    .into_bytes()
}

fn authority_intent_bytes(
    session_id: &str,
    operation: Identifier,
    authority_id: Identifier,
    proposed: &CommitPosition,
) -> Vec<u8> {
    format!(
        "{{\"schema_version\":1,\"session_id\":\"{session_id}\",\"operation_id\":\"{operation}\",\"kind\":\"session_create\",\"authority_id\":\"{authority_id}\",\"prior\":null,\"proposed\":{{\"storage_format\":\"event_log_v1\",\"log_generation\":\"{}\",\"through_seq\":{},\"through_event_id\":\"{}\",\"through_event_log_bytes\":{}}}}}\n",
        proposed.log_generation, proposed.through_seq, proposed.through_event_id, proposed.through_event_log_bytes
    )
    .into_bytes()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PositionWire {
    log_generation: String,
    through_seq: u64,
    through_event_id: String,
    through_event_log_bytes: u64,
}

impl PositionWire {
    fn into_position(self, file: &str) -> Result<CommitPosition, SessionError> {
        let bad = |what: &str| SessionError::InvalidFormat {
            file: file.into(),
            reason: format!("{what} is not 32 lowercase hex"),
        };
        Ok(CommitPosition {
            log_generation: Identifier::parse(&self.log_generation)
                .ok_or_else(|| bad("log_generation"))?,
            through_seq: self.through_seq,
            through_event_id: Identifier::parse(&self.through_event_id)
                .ok_or_else(|| bad("through_event_id"))?,
            through_event_log_bytes: self.through_event_log_bytes,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationIntentWire {
    schema_version: u64,
    session_id: String,
    operation_id: String,
    kind: String,
    prior: PositionWire,
    proposed: PositionWire,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityProposedWire {
    storage_format: String,
    log_generation: String,
    through_seq: u64,
    through_event_id: String,
    through_event_log_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityIntentWire {
    schema_version: u64,
    session_id: String,
    operation_id: String,
    kind: String,
    authority_id: String,
    prior: Option<serde_json::Value>,
    proposed: AuthorityProposedWire,
}

fn invalid(file: &str, reason: impl ToString) -> SessionError {
    SessionError::InvalidFormat {
        file: file.into(),
        reason: reason.to_string(),
    }
}

fn rollback_tail(log: &File, prior: &CommitPosition) -> Result<(), SessionError> {
    log.set_len(prior.through_event_log_bytes)?;
    log.sync_all()?;
    Ok(())
}

/// Resolves an `authority.pending.json` left by an interrupted `create`.
fn resolve_authority_intent(dir: &Path, session_id: &str) -> Result<(), SessionError> {
    if fs::symlink_metadata(dir.join(AUTHORITY_INTENT_FILE)).is_err() {
        return Ok(());
    }
    let bytes = read_bounded(dir, AUTHORITY_INTENT_FILE, INTENT_MAX_BYTES)?;
    let wire: AuthorityIntentWire =
        serde_json::from_slice(&bytes).map_err(|e| invalid(AUTHORITY_INTENT_FILE, e))?;
    if wire.schema_version != 1
        || wire.kind != "session_create"
        || wire.prior.is_some()
        || wire.session_id != session_id
    {
        return Err(invalid(
            AUTHORITY_INTENT_FILE,
            "not a session_create intent for this session",
        ));
    }
    Identifier::parse(&wire.operation_id)
        .ok_or_else(|| invalid(AUTHORITY_INTENT_FILE, "operation_id"))?;
    let authority_id = Identifier::parse(&wire.authority_id)
        .ok_or_else(|| invalid(AUTHORITY_INTENT_FILE, "authority_id"))?;
    if wire.proposed.storage_format != "event_log_v1" {
        return Err(invalid(AUTHORITY_INTENT_FILE, "storage_format"));
    }
    let proposed = PositionWire {
        log_generation: wire.proposed.log_generation,
        through_seq: wire.proposed.through_seq,
        through_event_id: wire.proposed.through_event_id,
        through_event_log_bytes: wire.proposed.through_event_log_bytes,
    }
    .into_position(AUTHORITY_INTENT_FILE)?;
    if fs::symlink_metadata(dir.join(AUTHORITY_FILE)).is_err() {
        // The authority marker never landed: the session was never fully created.
        sync_dir(dir)?;
        if fs::symlink_metadata(dir.join(AUTHORITY_FILE)).is_ok() {
            return Err(SessionError::CommitIndeterminate(
                "authority appeared while resolving its intent".into(),
            ));
        }
        delete_and_sync(dir, AUTHORITY_INTENT_FILE)?;
        return Err(SessionError::Missing(
            "session (creation was interrupted before its authority marker)".into(),
        ));
    }
    let authority = read_authority(dir, session_id)?;
    if authority.authority_id != authority_id {
        return Err(invalid(
            AUTHORITY_FILE,
            "authority_id differs from the creation intent",
        ));
    }
    validate_position(dir, proposed)?;
    sync_dir(dir)?;
    delete_and_sync(dir, AUTHORITY_INTENT_FILE)
}

/// Resolves a `commit.pending.json` left by an interrupted append to the prior or proposed
/// position: roll forward when the watermark names a valid proposed position, otherwise truncate
/// to the prior and republish it. Anything else is indeterminate and refused.
fn resolve_publication_intent(
    dir: &Path,
    session_id: &str,
    ids: &mut dyn IdSource,
) -> Result<(), SessionError> {
    if fs::symlink_metadata(dir.join(PUBLICATION_INTENT_FILE)).is_err() {
        return Ok(());
    }
    let bytes = read_bounded(dir, PUBLICATION_INTENT_FILE, INTENT_MAX_BYTES)?;
    let wire: PublicationIntentWire =
        serde_json::from_slice(&bytes).map_err(|e| invalid(PUBLICATION_INTENT_FILE, e))?;
    if wire.schema_version != 1 || wire.kind != "watermark_advance" || wire.session_id != session_id
    {
        return Err(invalid(
            PUBLICATION_INTENT_FILE,
            "not a watermark_advance intent for this session",
        ));
    }
    Identifier::parse(&wire.operation_id)
        .ok_or_else(|| invalid(PUBLICATION_INTENT_FILE, "operation_id"))?;
    let prior = wire.prior.into_position(PUBLICATION_INTENT_FILE)?;
    let proposed = wire.proposed.into_position(PUBLICATION_INTENT_FILE)?;
    let generation = live_generation(dir)?;
    if generation != prior.log_generation || generation != proposed.log_generation {
        return Err(SessionError::CommitIndeterminate(
            "the intent is for another log generation".into(),
        ));
    }
    let current = read_watermark(dir, session_id, generation)?;
    let log = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(EVENTS_FILE))?;
    if current == proposed {
        if validate_position(dir, proposed).is_err() {
            validate_position(dir, prior).map_err(|e| {
                SessionError::RecoveryFailed(format!("neither position is valid: {e}"))
            })?;
            rollback_tail(&log, &prior)?;
            durable_replace(
                dir,
                &watermark_file_name(generation),
                &encode_watermark(session_id, &prior),
                ids,
            )?;
            validate_position(dir, prior)?;
        }
    } else if current == prior {
        validate_position(dir, prior).map_err(|e| {
            SessionError::RecoveryFailed(format!("the prior position is not valid: {e}"))
        })?;
        let length = log.metadata()?.len();
        if length < prior.through_event_log_bytes {
            return Err(invalid(EVENTS_FILE, "shorter than the prior commit"));
        }
        if length > prior.through_event_log_bytes {
            rollback_tail(&log, &prior)?;
        }
        validate_position(dir, prior)?;
    } else {
        return Err(SessionError::CommitIndeterminate(
            "the watermark is neither the prior nor the proposed position".into(),
        ));
    }
    sync_dir(dir)?;
    delete_and_sync(dir, PUBLICATION_INTENT_FILE)
}

impl SessionWriter {
    /// Test-only: stop as if the process died at `boundary`.
    pub fn crash_at(&mut self, boundary: Option<Boundary>) {
        self.crash_at = boundary;
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn position(&self) -> CommitPosition {
        self.position
    }

    fn hit(&self, boundary: Boundary) -> Result<(), SessionError> {
        if self.crash_at == Some(boundary) {
            Err(SessionError::SimulatedCrash(boundary.name()))
        } else {
            Ok(())
        }
    }

    /// Creates `<sessions_dir>/<session_id>` with a first `session_started` event and the
    /// authority marker, following the Zig creation sequence. Fails with `AlreadyExists` rather than
    /// touching an existing session.
    pub fn create(
        sessions_dir: &Path,
        session_id: &str,
        first: NewEvent,
        mut ids: Box<dyn IdSource>,
        crash_at: Option<Boundary>,
    ) -> Result<Self, SessionError> {
        validate_session_id(session_id)?;
        if first.kind != Kind::SessionStarted {
            return Err(SessionError::InvalidFrame {
                offset: 0,
                reason: "the first event of a session is session_started".into(),
            });
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(sessions_dir)?;
        let dir = sessions_dir.join(session_id);
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(SessionError::AlreadyExists);
            }
            Err(e) => return Err(e.into()),
        }
        sync_dir(sessions_dir)?;
        private_dir_check(&dir)?;
        let session_lock = acquire_lock(&dir, SESSION_LOCK_FILE)?;
        drop(acquire_lock(&dir, COMMIT_LOCK_FILE)?);

        let generation = ids.next_id()?;
        let event_id = ids.next_id()?;
        let authority_id = ids.next_id()?;
        let frame = Frame {
            log_generation: generation,
            seq: 1,
            event_id,
            timestamp_ms: first.timestamp_ms,
            kind: first.kind,
            payload: first.payload,
        };
        let line = encode_frame(&frame)?;
        let log = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(EVENTS_FILE))?;
        log.write_all_at(&line, 0)?;
        let mut writer = Self {
            dir: dir.clone(),
            session_id: session_id.to_string(),
            position: CommitPosition {
                log_generation: generation,
                through_seq: 1,
                through_event_id: event_id,
                through_event_log_bytes: line.len() as u64,
            },
            log,
            ids,
            crash_at,
            _session_lock: session_lock,
        };
        writer.hit(Boundary::AfterEventAppend)?;
        writer.log.sync_all()?;
        writer.hit(Boundary::AfterEventSync)?;
        let position = writer.position;
        durable_replace(
            &dir,
            &watermark_file_name(generation),
            &encode_watermark(session_id, &position),
            writer.ids.as_mut(),
        )?;
        writer.hit(Boundary::AfterWatermarkRename)?;
        let _commit = acquire_lock(&dir, COMMIT_LOCK_FILE)?;
        let operation = writer.ids.next_id()?;
        durable_replace(
            &dir,
            AUTHORITY_INTENT_FILE,
            &authority_intent_bytes(session_id, operation, authority_id, &position),
            writer.ids.as_mut(),
        )?;
        writer.hit(Boundary::AfterAuthorityIntentSync)?;
        durable_replace(
            &dir,
            AUTHORITY_FILE,
            &encode_authority(session_id, authority_id),
            writer.ids.as_mut(),
        )?;
        writer.hit(Boundary::AfterAuthorityRename)?;
        delete_and_sync(&dir, AUTHORITY_INTENT_FILE)?;
        writer.hit(Boundary::AfterIntentRemove)?;
        Ok(writer)
    }

    /// Opens an existing session for writing: takes the session lock, requires private modes,
    /// resolves any interrupted creation or publication, and proves the committed log.
    pub fn open(
        session_dir: &Path,
        mut ids: Box<dyn IdSource>,
        crash_at: Option<Boundary>,
    ) -> Result<Self, SessionError> {
        let session_id = session_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or(SessionError::InvalidSessionId)?
            .to_string();
        validate_session_id(&session_id)?;
        private_dir_check(session_dir)?;
        let session_lock = acquire_lock(session_dir, SESSION_LOCK_FILE)?;
        {
            let _commit = acquire_lock(session_dir, COMMIT_LOCK_FILE)?;
            resolve_authority_intent(session_dir, &session_id)?;
            resolve_publication_intent(session_dir, &session_id, ids.as_mut())?;
        }
        for name in [EVENTS_FILE, AUTHORITY_FILE] {
            private_file_check(session_dir, name)?;
        }
        let generation = live_generation(session_dir)?;
        private_file_check(session_dir, &watermark_file_name(generation))?;
        let position = read_watermark(session_dir, &session_id, generation)?;
        validate_position(session_dir, position)?;
        read_authority(session_dir, &session_id)?;
        let log = OpenOptions::new()
            .read(true)
            .write(true)
            .open(open_regular(session_dir, EVENTS_FILE).map(|_| session_dir.join(EVENTS_FILE))?)?;
        Ok(Self {
            dir: session_dir.to_path_buf(),
            session_id,
            position,
            log,
            ids,
            crash_at,
            _session_lock: session_lock,
        })
    }

    /// Appends events as one atomic publication and returns the new committed position. On a
    /// failure before publication the tail is rolled back and nothing is published; after
    /// publication a failed check rolls back to the prior position. Never retried.
    pub fn append(&mut self, events: Vec<NewEvent>) -> Result<CommitPosition, SessionError> {
        if events.is_empty() {
            return Ok(self.position);
        }
        let prior = self.position;
        let mut bytes = Vec::new();
        let mut seq = prior.through_seq;
        let mut last_id = prior.through_event_id;
        for event in events {
            seq = seq
                .checked_add(1)
                .ok_or_else(|| invalid(EVENTS_FILE, "sequence overflow"))?;
            last_id = self.ids.next_id()?;
            bytes.extend(encode_frame(&Frame {
                log_generation: prior.log_generation,
                seq,
                event_id: last_id,
                timestamp_ms: event.timestamp_ms,
                kind: event.kind,
                payload: event.payload,
            })?);
        }
        let proposed = CommitPosition {
            log_generation: prior.log_generation,
            through_seq: seq,
            through_event_id: last_id,
            through_event_log_bytes: prior.through_event_log_bytes + bytes.len() as u64,
        };

        // 1. Write and sync the frames after the committed end, dropping any stale tail first.
        let length = self.log.metadata()?.len();
        if length < prior.through_event_log_bytes {
            return Err(invalid(EVENTS_FILE, "shorter than the committed position"));
        }
        let pre_intent = (|| -> Result<(), SessionError> {
            if length > prior.through_event_log_bytes {
                rollback_tail(&self.log, &prior)?;
            }
            self.log
                .write_all_at(&bytes, prior.through_event_log_bytes)?;
            self.hit(Boundary::AfterEventAppend)?;
            self.log.sync_all()?;
            self.hit(Boundary::AfterEventSync)
        })();
        if let Err(error) = pre_intent {
            return Err(self.fail_before_intent(error, &prior));
        }

        // 2. Under the commit lock: nobody moved the watermark, then intent, watermark, cleanup.
        let _commit = match acquire_lock(&self.dir, COMMIT_LOCK_FILE) {
            Ok(lock) => lock,
            Err(error) => return Err(self.fail_before_intent(error, &prior)),
        };
        let unchanged = read_watermark(&self.dir, &self.session_id, prior.log_generation)
            .map(|current| current == prior)
            .unwrap_or(false)
            && fs::symlink_metadata(self.dir.join(PUBLICATION_INTENT_FILE)).is_err();
        if !unchanged {
            return Err(self.fail_after_intent(
                SessionError::CommitIndeterminate("the watermark moved under this writer".into()),
                &prior,
            ));
        }
        let operation = self.ids.next_id()?;
        let publish = (|| -> Result<(), SessionError> {
            durable_replace(
                &self.dir,
                PUBLICATION_INTENT_FILE,
                &publication_intent_bytes(&self.session_id, operation, &prior, &proposed),
                self.ids.as_mut(),
            )?;
            self.hit(Boundary::AfterIntentSync)?;
            durable_replace(
                &self.dir,
                &watermark_file_name(proposed.log_generation),
                &encode_watermark(&self.session_id, &proposed),
                self.ids.as_mut(),
            )?;
            self.hit(Boundary::AfterWatermarkRename)
        })();
        if let Err(error) = publish {
            return Err(self.fail_after_intent(error, &prior));
        }

        // 3. The watermark and the appended bytes must read back exactly as proposed.
        let verified = (|| -> Result<(), SessionError> {
            if read_watermark(&self.dir, &self.session_id, proposed.log_generation)? != proposed {
                return Err(SessionError::CommitIndeterminate(
                    "the published watermark differs".into(),
                ));
            }
            let mut tail = vec![0u8; bytes.len()];
            self.log
                .read_exact_at(&mut tail, prior.through_event_log_bytes)?;
            if tail != bytes {
                return Err(SessionError::CommitIndeterminate(
                    "the appended bytes differ".into(),
                ));
            }
            Ok(())
        })();
        if let Err(error) = verified {
            return Err(self.fail_after_intent(error, &prior));
        }
        self.position = proposed;
        delete_and_sync(&self.dir, PUBLICATION_INTENT_FILE).map_err(|e| {
            SessionError::CommitIndeterminate(format!(
                "published, but the intent could not be removed: {e}"
            ))
        })?;
        self.hit(Boundary::AfterIntentRemove)?;
        Ok(proposed)
    }

    fn fail_before_intent(&self, error: SessionError, prior: &CommitPosition) -> SessionError {
        if matches!(error, SessionError::SimulatedCrash(_)) {
            return error;
        }
        match rollback_tail(&self.log, prior) {
            Ok(()) => SessionError::PersistenceDegraded(error.to_string()),
            Err(rollback) => {
                SessionError::CommitIndeterminate(format!("{error}; rollback failed: {rollback}"))
            }
        }
    }

    fn fail_after_intent(&mut self, error: SessionError, prior: &CommitPosition) -> SessionError {
        if matches!(error, SessionError::SimulatedCrash(_)) {
            return error;
        }
        let bytes = encode_watermark(&self.session_id, prior);
        let name = watermark_file_name(prior.log_generation);
        let rolled = rollback_tail(&self.log, prior)
            .and_then(|()| durable_replace(&self.dir, &name, &bytes, self.ids.as_mut()))
            .and_then(|()| validate_position(&self.dir, *prior))
            .and_then(|()| delete_and_sync(&self.dir, PUBLICATION_INTENT_FILE));
        match rolled {
            Ok(()) => SessionError::PersistenceDegraded(error.to_string()),
            Err(rollback) => {
                SessionError::CommitIndeterminate(format!("{error}; rollback failed: {rollback}"))
            }
        }
    }
}
