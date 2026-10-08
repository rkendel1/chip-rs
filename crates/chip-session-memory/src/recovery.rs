//! Checkpoints and reconstruction from persisted data.
//!
//! A checkpoint is a record naming the position (FeltDB's sequence number), FeltDB's own digest
//! of its state, and references to everything a recovery must find. Writing it uses FeltDB's
//! `Synced` durability so that it is behind a stable-storage barrier, then restores the mode.
//! Reconstruction reads only persisted records; it validates every reference and refuses an
//! inconsistent session rather than returning a partial one.

use std::path::Path;

use crate::backend::Backend;
use crate::error::{Result, SessionMemoryError as E};
use crate::schema::*;
use crate::store::SessionMemory;

/// Everything needed to understand, resume or escalate a session, rebuilt from storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredSession {
    pub session: SessionRecord,
    pub tasks: Vec<TaskRecord>,
    pub plan: Option<PlanRecord>,
    pub attempts: Vec<AttemptRecord>,
    /// Attempts that failed and must not be repeated blindly.
    pub failed_approaches: Vec<AttemptRecord>,
    pub repairs: Vec<RepairRecord>,
    /// Repairs that succeeded and are verified by a passing test result.
    pub verified_repairs: Vec<RepairRecord>,
    /// Current readings that are failing (not superseded by a later reading of the command).
    pub unresolved_failures: Vec<TestResultRecord>,
    pub hypotheses: Vec<HypothesisRecord>,
    pub open_hypotheses: Vec<HypothesisRecord>,
    pub escalations: Vec<EscalationRecord>,
    pub checkpoint: Option<CheckpointRecord>,
    /// A compaction was preserved but its purge had not finished.
    pub pending_compaction: Option<u32>,
    pub observations: u64,
    pub payloads_held: u64,
}

impl<B: Backend> SessionMemory<B> {
    /// Writes a checkpoint behind the engine's strongest durability barrier and records it on the
    /// session.
    pub fn checkpoint(&mut self, id: &str) -> Result<CheckpointRecord> {
        valid_id(id)?;
        let mut refs = Vec::new();
        let mut add = |kind: &str, id: String| {
            refs.push(Reference {
                kind: kind.into(),
                id,
            })
        };
        for a in self.list::<AttemptRecord>(coll::ATTEMPT)? {
            add(coll::ATTEMPT, a.id);
        }
        for r in self.list::<RepairRecord>(coll::REPAIR)? {
            add(coll::REPAIR, r.id);
        }
        for t in self.list::<TestResultRecord>(coll::TEST)? {
            add(coll::TEST, t.id);
        }
        for h in self.list::<HypothesisRecord>(coll::HYP)? {
            add(coll::HYP, h.id);
        }
        for e in self.list::<EscalationRecord>(coll::ESC)? {
            add(coll::ESC, e.id);
        }
        for t in self.list::<TaskRecord>(coll::TASK)? {
            add(coll::TASK, t.id);
        }
        if self.exists(coll::PLAN, "current")? {
            add(coll::PLAN, "current".into());
        }
        for o in self.observations()?.into_iter().filter(|o| o.pinned) {
            add(coll::OBS, o.id);
        }
        let (sequence, state_digest) = self.backend()?.position()?;
        let record = CheckpointRecord {
            schema: SCHEMA_VERSION,
            session: self.session().into(),
            id: id.into(),
            sequence,
            state_digest,
            refs,
        };
        let this = &*self;
        this.backend()?
            .synced(&mut || {
                this.put(coll::CKPT, id, &record).and_then(|_| {
                    let mut s = this.session_record()?;
                    s.last_checkpoint = Some(id.into());
                    this.put(coll::SESSION, "meta", &s)
                })
            })
            .map_err(|e| E::Checkpoint(e.to_string()))?;
        Ok(record)
    }

    /// Reopens a session from its persisted files and reconstructs it.
    pub fn recover_with(root: &Path, session: &str) -> Result<(Self, RecoveredSession)> {
        valid_id(session)?;
        let dir = root.join(session);
        if !dir.join(B::FILE).exists() {
            return Err(E::Recovery(format!(
                "no persisted session {session} under {}",
                root.display()
            )));
        }
        let me = Self::open_in(&dir, session).map_err(|e| E::Recovery(e.to_string()))?;
        let recovered = me.reconstruct()?;
        Ok((me, recovered))
    }

    /// Rebuilds the session from persisted records and checks that every reference resolves.
    pub fn reconstruct(&self) -> Result<RecoveredSession> {
        let bad = |m: String| E::Recovery(m);
        let session = self
            .read::<SessionRecord>(coll::SESSION, "meta")
            .map_err(|e| bad(e.to_string()))?
            .ok_or_else(|| bad("the session record is missing".into()))?;
        if session.schema > SCHEMA_VERSION {
            return Err(bad(format!(
                "schema {} is newer than this build understands ({SCHEMA_VERSION})",
                session.schema
            )));
        }
        if session.session != self.session() {
            return Err(bad(format!(
                "store holds session {}, expected {}",
                session.session,
                self.session()
            )));
        }
        let q = |e: E| bad(e.to_string());
        let tasks = self.list::<TaskRecord>(coll::TASK).map_err(q)?;
        let plan = self.read::<PlanRecord>(coll::PLAN, "current").map_err(q)?;
        let attempts = self.list::<AttemptRecord>(coll::ATTEMPT).map_err(q)?;
        let repairs = self.list::<RepairRecord>(coll::REPAIR).map_err(q)?;
        let tests = self.test_results().map_err(q)?;
        let hypotheses = self.list::<HypothesisRecord>(coll::HYP).map_err(q)?;
        let escalations = self.list::<EscalationRecord>(coll::ESC).map_err(q)?;
        let observations = self.observations().map_err(q)?;

        let has = |kind: &str, id: &str| -> bool {
            match kind {
                coll::ATTEMPT => attempts.iter().any(|x| x.id == id),
                coll::REPAIR => repairs.iter().any(|x| x.id == id),
                coll::TEST => tests.iter().any(|x| x.id == id),
                coll::HYP => hypotheses.iter().any(|x| x.id == id),
                coll::ESC => escalations.iter().any(|x| x.id == id),
                coll::TASK => tasks.iter().any(|x| x.id == id),
                coll::PLAN => plan.is_some(),
                coll::OBS => observations.iter().any(|x| x.id == id),
                _ => false,
            }
        };
        for a in &attempts {
            for r in &a.refs {
                if !has(&r.kind, &r.id) {
                    return Err(bad(format!(
                        "attempt {} references missing {} {}",
                        a.id, r.kind, r.id
                    )));
                }
            }
        }
        for r in &repairs {
            if let Some(t) = &r.verified_by {
                match tests.iter().find(|x| &x.id == t) {
                    Some(x) if x.passed => {}
                    Some(_) => {
                        return Err(bad(format!(
                            "repair {} is verified by test result {t}, which did not pass",
                            r.id
                        )));
                    }
                    None => {
                        return Err(bad(format!(
                            "repair {} is verified by missing test result {t}",
                            r.id
                        )));
                    }
                }
            }
        }
        for t in &tests {
            if let Some(d) = &t.diagnostic {
                if !has(coll::OBS, d) {
                    return Err(bad(format!(
                        "test result {} cites missing observation {d}",
                        t.id
                    )));
                }
            }
        }
        for h in &hypotheses {
            for r in &h.evidence {
                if !has(&r.kind, &r.id) {
                    return Err(bad(format!(
                        "hypothesis {} cites missing {} {}",
                        h.id, r.kind, r.id
                    )));
                }
            }
        }
        for e in &escalations {
            for a in &e.prior_attempts {
                if !has(coll::ATTEMPT, a) {
                    return Err(bad(format!(
                        "escalation {} cites missing attempt {a}",
                        e.id
                    )));
                }
            }
        }
        // Every pinned observation must still hold the payload it was pinned to keep.
        for o in observations.iter().filter(|o| o.pinned && o.payload_held) {
            if self.payload(&o.id).map_err(q)?.is_none() {
                return Err(bad(format!("pinned observation {} lost its payload", o.id)));
            }
        }
        let checkpoint = match &session.last_checkpoint {
            Some(id) => {
                let c: CheckpointRecord = self
                    .read(coll::CKPT, id)
                    .map_err(q)?
                    .ok_or_else(|| bad(format!("checkpoint {id} is missing")))?;
                for r in &c.refs {
                    if !has(&r.kind, &r.id) {
                        return Err(bad(format!(
                            "checkpoint {id} references missing {} {}",
                            r.kind, r.id
                        )));
                    }
                }
                Some(c)
            }
            None => None,
        };
        let pending_compaction = {
            let mut all: Vec<CompactionRecord> = self.list(coll::COMPACTION).map_err(q)?;
            all.sort_by_key(|c| c.number);
            all.pop()
                .filter(|c| c.state == CompactionState::Preserved)
                .map(|c| c.number)
        };
        let payloads_held = observations.iter().filter(|o| o.payload_held).count() as u64;
        Ok(RecoveredSession {
            failed_approaches: attempts
                .iter()
                .filter(|a| a.outcome == Outcome::Failed)
                .cloned()
                .collect(),
            verified_repairs: repairs
                .iter()
                .filter(|r| r.outcome == Outcome::Succeeded && r.verified_by.is_some())
                .cloned()
                .collect(),
            unresolved_failures: tests
                .iter()
                .filter(|t| !t.passed && t.superseded_by.is_none())
                .cloned()
                .collect(),
            open_hypotheses: hypotheses.iter().filter(|h| h.open).cloned().collect(),
            observations: observations.len() as u64,
            payloads_held,
            session,
            tasks,
            plan,
            attempts,
            repairs,
            hypotheses,
            escalations,
            checkpoint,
            pending_compaction,
        })
    }
}
