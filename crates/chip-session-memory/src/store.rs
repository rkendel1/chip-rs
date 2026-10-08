//! The experimental session-memory adapter: one FeltDB store per session.
//!
//! Isolation is structural: a session is its own journal file in its own directory, so one
//! session's adapter holds no handle to another's data. Every key and record also carries the
//! session id, and a record that names another session is refused rather than returned.
//!
//! Lifecycle: [`SessionMemory::create`] makes a store, [`SessionMemory::recover`] reopens one from
//! its journal, [`SessionMemory::close`] drops the handle (FeltDB has no `close()`; dropping the
//! last handle is its supported release and also releases its ownership lock), and
//! [`SessionMemory::destroy`] closes and deletes the files. `close` is idempotent. Operations on
//! a closed session return [`SessionMemoryError::Closed`].
//!
//! Concurrency: FeltDB serializes individual operations on one store with an internal mutex. The
//! multi-step procedures here (compaction, checkpoint) are not isolated from concurrent writers
//! to the same session, so every mutating method takes `&mut self` and a session is single-writer.
//! Independent sessions are independent stores and may be used from different threads.

use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::backend::{Backend, Felt};
use crate::error::{Result, SessionMemoryError as E};
use crate::schema::*;

/// What the native FeltDB Rust API offers for this use, stated rather than assumed. A capability
/// that is not offered is reported, never emulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    Supported,
    Unsupported(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub in_memory_backend: Support,
    pub durable_file_journal: Support,
    pub explicit_close: Support,
    pub atomic_batches: Support,
    pub physical_reclaim_without_replication: Support,
    pub concurrent_writers_to_one_session: Support,
}

pub fn capabilities() -> Capabilities {
    Capabilities {
        in_memory_backend: Support::Unsupported(
            "FeltDb::open takes a path; the Rust core has no in-memory backend (MemoryStorage is an unused log helper and StateStore::new_volatile is a revision-model test double)",
        ),
        durable_file_journal: Support::Supported,
        explicit_close: Support::Unsupported(
            "there is no close(); dropping the last handle releases the store",
        ),
        atomic_batches: Support::Supported,
        physical_reclaim_without_replication: Support::Unsupported(
            "compact_operation_log prunes only operations a configured peer has acknowledged; with no peers it returns immediately",
        ),
        concurrent_writers_to_one_session: Support::Unsupported(
            "individual operations are mutex-serialized, but multi-step procedures are not isolated and this was not tested concurrently",
        ),
    }
}

/// A session's working memory over storage candidate `B` (FeltDB unless stated).
pub struct SessionMemory<B: Backend = Felt> {
    pub(crate) backend: Option<B>,
    pub(crate) dir: PathBuf,
    pub(crate) session: String,
}

pub(crate) fn sha256_hex(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The first `max` bytes of `text`, cut at a character boundary.
pub(crate) fn excerpt(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// What the caller supplies for an observation.
#[derive(Debug, Clone)]
pub struct NewObservation {
    pub id: String,
    pub kind: String,
    pub provenance: Option<String>,
    pub summary: String,
    /// The full output. It is transient: it may be stored (and later purged) or dropped.
    pub payload: Option<String>,
    pub pinned: bool,
    pub superseded_by: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Live records per collection, as FeltDB tracks them (revision and retention rows included).
    pub live_rows: BTreeMap<String, u64>,
    /// Records the session itself wrote, per collection.
    pub records: BTreeMap<String, u64>,
    /// Sum of the serialized size of the session's own records, payloads excluded.
    pub record_bytes: u64,
    pub payload_records: u64,
    pub payload_bytes: u64,
    pub journal_bytes: u64,
}

impl SessionMemory<Felt> {
    /// Creates a FeltDB-backed session; see [`create_with`](SessionMemory::create_with).
    pub fn create(root: &Path, session: &str, objective: &str) -> Result<Self> {
        Self::create_with(root, session, objective)
    }

    /// Reopens a FeltDB-backed session from its journal and reconstructs it.
    pub fn recover(
        root: &Path,
        session: &str,
    ) -> Result<(Self, crate::recovery::RecoveredSession)> {
        Self::recover_with(root, session)
    }

    /// Like [`create`](Self::create), but fails after the store is open and written, to exercise
    /// the cleanup of a partially created session.
    #[doc(hidden)]
    pub fn create_failing_after_open(root: &Path, session: &str, objective: &str) -> Result<Self> {
        Self::create_inner(root, session, objective, true)
    }

    pub fn journal_path(&self) -> PathBuf {
        self.dir.join(Felt::FILE)
    }
}

impl<B: Backend> SessionMemory<B> {
    // ------------------------------------------------------------------ lifecycle

    /// Creates a new session store under `root/<session>/`. Refuses an existing session. If
    /// anything fails after the directory was created, the directory is removed.
    pub fn create_with(root: &Path, session: &str, objective: &str) -> Result<Self> {
        Self::create_inner(root, session, objective, false)
    }

    fn create_inner(
        root: &Path,
        session: &str,
        objective: &str,
        fail_after_open: bool,
    ) -> Result<Self> {
        valid_id(session)?;
        if objective.trim().is_empty() {
            return Err(E::Invalid("a session needs an objective".into()));
        }
        std::fs::create_dir_all(root).map_err(|e| E::Init(format!("{}: {e}", root.display())))?;
        let dir = root.join(session);
        std::fs::create_dir(&dir).map_err(|e| E::Init(format!("{}: {e}", dir.display())))?;
        let built = Self::open_in(&dir, session).and_then(|m| {
            let record = SessionRecord {
                schema: SCHEMA_VERSION,
                session: session.into(),
                objective: objective.into(),
                status: SessionStatus::Active,
                pending: None,
                last_checkpoint: None,
                compactions: 0,
            };
            // Creation is a barrier like a checkpoint: a session whose own record could be lost
            // by a crash would not be recoverable at all.
            let value = serde_json::to_value(&record).map_err(|e| E::Init(e.to_string()))?;
            m.backend()?
                .synced(&mut || m.backend()?.put(coll::SESSION, "meta", &value))
                .map_err(|e| E::Init(e.to_string()))?;
            if fail_after_open {
                return Err(E::Init(
                    "injected failure after the store was opened".into(),
                ));
            }
            Ok(m)
        });
        match built {
            Ok(m) => Ok(m),
            Err(e) => {
                // Only a directory this call created is removed.
                let _ = std::fs::remove_dir_all(&dir);
                Err(e)
            }
        }
    }

    pub(crate) fn open_in(dir: &Path, session: &str) -> Result<Self> {
        let backend = B::open(dir, session)?;
        Ok(Self {
            backend: Some(backend),
            dir: dir.to_path_buf(),
            session: session.into(),
        })
    }

    /// Closes the store. Idempotent: returns whether this call did the closing.
    pub fn close(&mut self) -> bool {
        self.backend.take().is_some()
    }

    pub fn is_closed(&self) -> bool {
        self.backend.is_none()
    }

    /// Closes the store and deletes its files.
    pub fn destroy(mut self) -> Result<()> {
        self.close();
        std::fs::remove_dir_all(&self.dir)
            .map_err(|e| E::Persist(format!("{}: {e}", self.dir.display())))
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The storage engine, for tests that need to damage or inspect persisted data directly.
    #[doc(hidden)]
    pub fn backend(&self) -> Result<&B> {
        self.backend.as_ref().ok_or(E::Closed)
    }

    // ------------------------------------------------------------------ generic access

    pub(crate) fn put<T: Serialize>(&self, collection: &str, id: &str, value: &T) -> Result<()> {
        let v = serde_json::to_value(value).map_err(|e| E::Persist(e.to_string()))?;
        self.backend()?.put(collection, id, &v)
    }

    pub(crate) fn read<T: DeserializeOwned>(
        &self,
        collection: &str,
        id: &str,
    ) -> Result<Option<T>> {
        let Some(value) = self.backend()?.get(collection, id)? else {
            return Ok(None);
        };
        self.check_session(&value)?;
        serde_json::from_value(value)
            .map(Some)
            .map_err(|e| E::Query(format!("{collection}:{id}: {e}")))
    }

    pub(crate) fn check_session(&self, value: &serde_json::Value) -> Result<()> {
        match value.get("session").and_then(|s| s.as_str()) {
            Some(s) if s == self.session => Ok(()),
            Some(s) => Err(E::ForeignRecord {
                expected: self.session.clone(),
                found: s.into(),
            }),
            None => Err(E::Query("a record has no session field".into())),
        }
    }

    pub(crate) fn exists(&self, collection: &str, id: &str) -> Result<bool> {
        Ok(self.backend()?.get(collection, id)?.is_some())
    }

    /// Every record of a collection, in id order. Foreign records are an error.
    pub(crate) fn list<T: DeserializeOwned>(&self, collection: &str) -> Result<Vec<T>> {
        let mut out = Vec::new();
        self.backend()?.scan(collection, &mut |id, value| {
            self.check_session(value)?;
            out.push(
                serde_json::from_value(value.clone())
                    .map_err(|e| E::Query(format!("{collection}:{id}: {e}")))?,
            );
            Ok(())
        })?;
        Ok(out)
    }

    fn require(&self, collection: &'static str, id: &str) -> Result<()> {
        if self.exists(collection, id)? {
            Ok(())
        } else {
            Err(E::MissingReference {
                kind: collection,
                id: id.into(),
            })
        }
    }

    fn require_refs(&self, refs: &[Reference]) -> Result<()> {
        for r in refs {
            let collection = coll::ALL
                .iter()
                .copied()
                .find(|c| *c == r.kind)
                .ok_or_else(|| E::Invalid(format!("unknown reference kind {:?}", r.kind)))?;
            valid_id(&r.id)?;
            self.require(collection, &r.id)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ session, task, plan

    pub fn session_record(&self) -> Result<SessionRecord> {
        self.read(coll::SESSION, "meta")?
            .ok_or_else(|| E::Query("the session record is missing".into()))
    }

    pub fn set_status(&mut self, status: SessionStatus) -> Result<()> {
        let mut s = self.session_record()?;
        s.status = status;
        self.put(coll::SESSION, "meta", &s)
    }

    pub fn set_pending(&mut self, pending: Option<PendingAction>) -> Result<()> {
        let mut s = self.session_record()?;
        s.pending = pending;
        self.put(coll::SESSION, "meta", &s)
    }

    pub fn put_task(&mut self, id: &str, goal: &str, done: bool) -> Result<()> {
        valid_id(id)?;
        let r = TaskRecord {
            schema: SCHEMA_VERSION,
            session: self.session.clone(),
            id: id.into(),
            goal: goal.into(),
            done,
        };
        self.put(coll::TASK, id, &r)
    }

    pub fn set_plan(&mut self, steps: Vec<String>) -> Result<u32> {
        let revision = self
            .read::<PlanRecord>(coll::PLAN, "current")?
            .map_or(1, |p| p.revision + 1);
        let r = PlanRecord {
            schema: SCHEMA_VERSION,
            session: self.session.clone(),
            revision,
            steps,
        };
        self.put(coll::PLAN, "current", &r)?;
        Ok(revision)
    }

    /// Deletes a record. Only working-class records and transient payloads may be deleted this
    /// way; recovery-class records (attempts, repairs, test results, escalations, checkpoints,
    /// observations) are never deleted by a caller, only by compaction's rules.
    pub fn delete(&mut self, collection: &str, id: &str) -> Result<()> {
        valid_id(id)?;
        if !matches!(collection, coll::TASK | coll::HYP | coll::PAYLOAD) {
            return Err(E::Invalid(format!(
                "{collection} records are recovery state and are not deletable"
            )));
        }
        if collection == coll::PAYLOAD {
            // A record must never claim a payload that is not there.
            if let Some(mut o) = self.read::<ObservationRecord>(coll::OBS, id)? {
                if o.pinned {
                    return Err(E::Invalid(format!(
                        "the payload of pinned observation {id} is required by recovery"
                    )));
                }
                o.payload_held = false;
                self.put(coll::OBS, id, &o)?;
            }
        }
        self.backend()?.delete(collection, id)
    }

    // ------------------------------------------------------------------ attempts and observations

    pub fn record_attempt(
        &mut self,
        id: &str,
        action: &str,
        outcome: Outcome,
        refs: Vec<Reference>,
    ) -> Result<()> {
        valid_id(id)?;
        self.require_refs(&refs)?;
        let r = AttemptRecord {
            schema: SCHEMA_VERSION,
            session: self.session.clone(),
            id: id.into(),
            action: action.into(),
            outcome,
            refs,
        };
        self.put(coll::ATTEMPT, id, &r)
    }

    /// Records an observation. When `store_payload` is false the payload is dropped after its
    /// digest, length and excerpt have been taken: the record then says what was seen without
    /// keeping it.
    pub fn record_observation(&mut self, obs: NewObservation, store_payload: bool) -> Result<()> {
        valid_id(&obs.id)?;
        if let Some(s) = &obs.superseded_by {
            valid_id(s)?;
        }
        let (digest, len, ex) = match &obs.payload {
            Some(p) => (sha256_hex(p), p.len() as u64, excerpt(p, 256)),
            None => (sha256_hex(""), 0, String::new()),
        };
        let hold = store_payload && obs.payload.is_some();
        let record = ObservationRecord {
            schema: SCHEMA_VERSION,
            session: self.session.clone(),
            id: obs.id.clone(),
            kind: obs.kind,
            provenance: obs.provenance,
            summary: obs.summary,
            digest,
            payload_len: len,
            excerpt: ex,
            payload_held: hold,
            superseded_by: obs.superseded_by,
            pinned: obs.pinned,
        };
        // The payload is written first: a record never claims a payload that is not there.
        if hold {
            let p = PayloadRecord {
                schema: SCHEMA_VERSION,
                session: self.session.clone(),
                id: obs.id.clone(),
                text: obs.payload.unwrap_or_default(),
            };
            self.put(coll::PAYLOAD, &obs.id, &p)?;
        }
        self.put(coll::OBS, &obs.id, &record)
    }

    /// Marks `old` as superseded by `new`. Both must exist.
    pub fn supersede_observation(&mut self, old: &str, new: &str) -> Result<()> {
        self.require(coll::OBS, new)?;
        let mut o: ObservationRecord = self.read(coll::OBS, old)?.ok_or(E::MissingReference {
            kind: coll::OBS,
            id: old.into(),
        })?;
        o.superseded_by = Some(new.into());
        self.put(coll::OBS, old, &o)
    }

    pub fn observation(&self, id: &str) -> Result<Option<ObservationRecord>> {
        self.read(coll::OBS, id)
    }

    pub fn payload(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .read::<PayloadRecord>(coll::PAYLOAD, id)?
            .map(|p| p.text))
    }

    /// Every stored payload's id and length, paged so that only a page of payloads is ever
    /// materialized at once.
    pub(crate) fn payload_index(&self) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        self.backend()?.scan(coll::PAYLOAD, &mut |_, value| {
            self.check_session(value)?;
            let id = value
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let len = value
                .get("text")
                .and_then(|v| v.as_str())
                .map_or(0, str::len) as u64;
            out.push((id, len));
            Ok(())
        })?;
        Ok(out)
    }

    pub fn observations(&self) -> Result<Vec<ObservationRecord>> {
        self.list(coll::OBS)
    }

    // ------------------------------------------------------------------ tests, repairs, hypotheses

    pub fn record_test_result(
        &mut self,
        id: &str,
        command: &str,
        passed: bool,
        failed_tests: Vec<String>,
        diagnostic: Option<&str>,
    ) -> Result<()> {
        valid_id(id)?;
        if let Some(d) = diagnostic {
            self.require(coll::OBS, d)?;
        }
        // A newer reading of the same command supersedes the older one.
        for mut prior in self.test_results()? {
            if prior.command == command && prior.superseded_by.is_none() && prior.id != id {
                prior.superseded_by = Some(id.into());
                self.put(coll::TEST, &prior.id.clone(), &prior)?;
            }
        }
        let r = TestResultRecord {
            schema: SCHEMA_VERSION,
            session: self.session.clone(),
            id: id.into(),
            command: command.into(),
            passed,
            failed_tests,
            diagnostic: diagnostic.map(Into::into),
            superseded_by: None,
        };
        self.put(coll::TEST, id, &r)
    }

    pub fn test_results(&self) -> Result<Vec<TestResultRecord>> {
        self.list(coll::TEST)
    }

    /// Records a repair. `verified_by` is accepted only if it names a recorded test result that
    /// passed: a repair cannot be marked verified by saying so.
    pub fn record_repair(
        &mut self,
        id: &str,
        fix: &str,
        outcome: Outcome,
        verified_by: Option<&str>,
    ) -> Result<()> {
        valid_id(id)?;
        if let Some(t) = verified_by {
            let result: TestResultRecord =
                self.read(coll::TEST, t)?.ok_or(E::MissingReference {
                    kind: coll::TEST,
                    id: t.into(),
                })?;
            if !result.passed {
                return Err(E::Invalid(format!(
                    "test result {t} did not pass, so it cannot verify a repair"
                )));
            }
        }
        let r = RepairRecord {
            schema: SCHEMA_VERSION,
            session: self.session.clone(),
            id: id.into(),
            fix: fix.into(),
            outcome,
            verified_by: verified_by.map(Into::into),
        };
        self.put(coll::REPAIR, id, &r)
    }

    pub fn record_hypothesis(
        &mut self,
        id: &str,
        explanation: &str,
        evidence: Vec<Reference>,
        confidence_permille: Option<u16>,
    ) -> Result<()> {
        valid_id(id)?;
        self.require_refs(&evidence)?;
        if confidence_permille.is_some_and(|c| c > 1000) {
            return Err(E::Invalid(
                "confidence is in parts per thousand, at most 1000".into(),
            ));
        }
        let r = HypothesisRecord {
            schema: SCHEMA_VERSION,
            session: self.session.clone(),
            id: id.into(),
            explanation: explanation.into(),
            evidence,
            confidence_permille,
            open: true,
        };
        self.put(coll::HYP, id, &r)
    }

    pub fn resolve_hypothesis(&mut self, id: &str) -> Result<()> {
        let mut h: HypothesisRecord = self.read(coll::HYP, id)?.ok_or(E::MissingReference {
            kind: coll::HYP,
            id: id.into(),
        })?;
        h.open = false;
        self.put(coll::HYP, id, &h)
    }

    pub fn record_escalation(
        &mut self,
        id: &str,
        reason: &str,
        prior_attempts: Vec<String>,
        known_failures: Vec<String>,
        successes: Vec<String>,
        outstanding: Vec<String>,
    ) -> Result<()> {
        valid_id(id)?;
        for a in &prior_attempts {
            self.require(coll::ATTEMPT, a)?;
        }
        let r = EscalationRecord {
            schema: SCHEMA_VERSION,
            session: self.session.clone(),
            id: id.into(),
            reason: reason.into(),
            prior_attempts,
            known_failures,
            successes,
            outstanding,
        };
        self.put(coll::ESC, id, &r)?;
        self.set_status(SessionStatus::Escalated)
    }

    // ------------------------------------------------------------------ statistics

    pub fn stats(&self) -> Result<Stats> {
        let backend = self.backend()?;
        let mut s = Stats {
            live_rows: backend.cardinalities()?,
            ..Default::default()
        };
        for c in coll::ALL {
            let mut count = 0u64;
            backend.scan(c, &mut |_, value| {
                count += 1;
                if c == coll::PAYLOAD {
                    s.payload_records += 1;
                    s.payload_bytes += value
                        .get("text")
                        .and_then(|t| t.as_str())
                        .map_or(0, str::len) as u64;
                } else {
                    s.record_bytes += value.to_string().len() as u64;
                }
                Ok(())
            })?;
            if count > 0 {
                s.records.insert(c.to_string(), count);
            }
        }
        s.journal_bytes = backend.disk_bytes();
        Ok(s)
    }
}

impl<B: Backend> SessionMemory<B> {
    /// Applies one workload event. `store_payload` says whether observation payloads are kept in
    /// the store (and so may later be purged) or dropped after their digest is taken.
    pub fn apply(&mut self, event: crate::workload::Event, store_payload: bool) -> Result<()> {
        use crate::workload::Event as Ev;
        match event {
            Ev::Task { id, goal } => self.put_task(&id, &goal, false),
            Ev::Plan { steps } => self.set_plan(steps).map(|_| ()),
            Ev::Observation {
                id,
                kind,
                provenance,
                summary,
                payload,
                ..
            } => self.record_observation(
                NewObservation {
                    id,
                    kind,
                    provenance: Some(provenance),
                    summary,
                    payload: Some(payload),
                    pinned: false,
                    superseded_by: None,
                },
                store_payload,
            ),
            Ev::Supersede { old, by } => self.supersede_observation(&old, &by),
            Ev::Attempt {
                id,
                action,
                outcome,
                refs,
            } => self.record_attempt(&id, &action, outcome, refs),
            Ev::TestResult {
                id,
                command,
                passed,
                failed_tests,
                diagnostic,
            } => self.record_test_result(&id, &command, passed, failed_tests, Some(&diagnostic)),
            Ev::Repair {
                id,
                fix,
                outcome,
                verified_by,
            } => self.record_repair(&id, &fix, outcome, verified_by.as_deref()),
            Ev::Hypothesis {
                id,
                explanation,
                evidence,
            } => self.record_hypothesis(&id, &explanation, evidence, None),
            Ev::ResolveHypothesis { id } => self.resolve_hypothesis(&id),
            Ev::Escalation {
                id,
                reason,
                prior_attempts,
                known_failures,
                successes,
                outstanding,
            } => self.record_escalation(
                &id,
                &reason,
                prior_attempts,
                known_failures,
                successes,
                outstanding,
            ),
            Ev::Pending(p) => self.set_pending(Some(p)),
            Ev::Checkpoint { id } => self.checkpoint(&id).map(|_| ()),
        }
    }
}
