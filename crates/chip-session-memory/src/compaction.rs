//! Retention classes, compaction and reclamation.
//!
//! **Classes.** *Working*: the plan, open tasks, open hypotheses. *Recovery*: attempts, repairs,
//! test results, escalations, checkpoints, and every observation's small record (summary, digest,
//! length, excerpt, provenance). *Transient*: the full payload of an observation. A payload that
//! recovery still needs is *pinned*: the diagnostic of a test failure that is still the current
//! reading of its command, and any observation an open hypothesis cites.
//!
//! **Procedure** (deterministic, idempotent, resumable):
//!
//! 1. *Select* the observations whose payload is held and not pinned.
//! 2. *Preserve*: check each one's recoverable information (summary, digest, length, and that the
//!    stored payload matches its digest) and persist a compaction record in state `Preserved`
//!    naming exactly what will be purged.
//! 3. *Validate*: read the record back and check that the session can be reconstructed from what
//!    will remain.
//! 4. *Purge*: delete payloads, in atomic batches that also mark the observation record.
//! 5. *Verify* the resulting logical state.
//! 6. *Record* the outcome (`Purged`).
//! 7. *Reclaim* (a separate step, reported separately): see [`SessionMemory::reclaim`].
//! 8. Only then is the session's `compactions` counter advanced.
//!
//! Interrupted at any point, a rerun finishes the work: a `Preserved` record is resumed, a
//! partial purge continues, and a session whose payloads are all gone reports that and does no
//! purge. Nothing is deleted before the compaction record naming it has been written and read
//! back.
//!
//! **Deleting is not reclaiming.** Deleting a payload makes it logically absent. FeltDB keeps the
//! deleted payload alive in its revision rows and in its in-memory operation log, and the journal
//! file keeps every record ever appended until it is rewritten. [`SessionMemory::reclaim`] uses
//! the only public mechanisms that release them and reports what each one did.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use feltdb::{AtomicMutation, FlowError, StateStore};

use crate::error::{CompactionPhase as P, Result, SessionMemoryError as E};
use crate::schema::*;
use crate::store::SessionMemory;

/// Test seam: where to stop a compaction, as an interrupted process would.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[doc(hidden)]
pub enum Inject {
    #[default]
    None,
    /// After the compaction record is written and validated, before any payload is deleted.
    AfterPreserve,
    /// After this many purge batches.
    DuringPurge { batches: usize },
    /// After the purge is verified, before its outcome is recorded.
    BeforeRecord,
    /// After the purge is verified and recorded, before reclamation.
    BeforeReclaim,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReclaimReport {
    /// Revision rows FeltDB's garbage collection removed.
    pub revisions_collected: usize,
    /// Operations pruned from FeltDB's operation log by acknowledged compaction.
    pub operations_pruned: usize,
    pub journal_before: u64,
    pub journal_after: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionReport {
    pub number: u32,
    /// There was nothing left to purge.
    pub nothing_to_purge: bool,
    pub resumed: bool,
    pub payloads_purged: u64,
    pub payload_bytes_purged: u64,
    pub payloads_pinned: u64,
    pub payload_bytes_pinned: u64,
    pub preserved_records: u64,
    pub preserved_bytes: u64,
    pub reclaim: ReclaimReport,
    pub elapsed: Duration,
}

impl SessionMemory {
    fn pinned_observation_ids(&self) -> Result<BTreeSet<String>> {
        let mut pinned = BTreeSet::new();
        for t in self.test_results()? {
            if !t.passed && t.superseded_by.is_none() {
                pinned.extend(t.diagnostic);
            }
        }
        for h in self.list::<HypothesisRecord>(coll::HYP)? {
            if h.open {
                pinned.extend(
                    h.evidence
                        .into_iter()
                        .filter(|r| r.kind == coll::OBS)
                        .map(|r| r.id),
                );
            }
        }
        Ok(pinned)
    }

    /// Runs the whole procedure, reclamation included.
    pub fn compact(&mut self) -> Result<CompactionReport> {
        self.compact_with(Inject::None)
    }

    #[doc(hidden)]
    pub fn compact_with(&mut self, inject: Inject) -> Result<CompactionReport> {
        let started = Instant::now();
        let fail = |phase: P| {
            move |e: E| match e {
                E::Compaction { .. } | E::Closed => e,
                other => E::Compaction {
                    phase,
                    message: other.to_string(),
                },
            }
        };

        // 1. Select. Pin what recovery still needs first, so selection never sees it as eligible.
        let pinned_ids = self.pinned_observation_ids().map_err(fail(P::Select))?;
        let mut observations: Vec<ObservationRecord> =
            self.observations().map_err(fail(P::Select))?;
        for o in observations
            .iter_mut()
            .filter(|o| pinned_ids.contains(&o.id) && !o.pinned)
        {
            o.pinned = true;
            self.put(coll::OBS, &o.id.clone(), o)
                .map_err(fail(P::Select))?;
        }
        let existing: Option<CompactionRecord> =
            self.latest_compaction().map_err(fail(P::Select))?;
        let resumed = existing
            .as_ref()
            .is_some_and(|c| c.state == CompactionState::Preserved);
        let number = match &existing {
            Some(c) if c.state == CompactionState::Preserved => c.number,
            Some(c) => c.number + 1,
            None => 1,
        };
        let held: BTreeMap<&str, &ObservationRecord> = observations
            .iter()
            .filter(|o| o.payload_held)
            .map(|o| (o.id.as_str(), o))
            .collect();
        let eligible: Vec<&ObservationRecord> =
            held.values().copied().filter(|o| !o.pinned).collect();
        let pinned_held: Vec<&ObservationRecord> =
            held.values().copied().filter(|o| o.pinned).collect();

        let claimed: BTreeSet<&str> = held.keys().copied().collect();
        let all_payloads = self.payload_index().map_err(fail(P::Select))?;
        let orphans: Vec<String> = all_payloads
            .iter()
            .filter(|(id, _)| !claimed.contains(id.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        let orphan_bytes: u64 = all_payloads
            .iter()
            .filter(|(id, _)| !claimed.contains(id.as_str()))
            .map(|(_, len)| *len)
            .sum();

        let mut report = CompactionReport {
            number,
            resumed,
            payloads_pinned: pinned_held.len() as u64,
            payload_bytes_pinned: pinned_held.iter().map(|o| o.payload_len).sum(),
            ..Default::default()
        };
        report.nothing_to_purge = eligible.is_empty() && orphans.is_empty() && !resumed;

        // 2. Preserve (only when there is something to purge and no record names it yet).
        let purge: Vec<String> = match &existing {
            Some(c) if resumed => c
                .purge
                .iter()
                .filter(|id| held.get(id.as_str()).is_some())
                .cloned()
                .collect(),
            _ => eligible.iter().map(|o| o.id.clone()).collect(),
        };
        let work = !purge.is_empty() || !orphans.is_empty();
        let active = work || resumed;
        if work && !resumed {
            let mut purge_bytes = 0u64;
            for id in &purge {
                let o = held[id.as_str()];
                // What recovery will have after the purge must already be in the record.
                if o.summary.trim().is_empty() || o.digest.len() != 64 {
                    return Err(E::Compaction {
                        phase: P::Preserve,
                        message: format!("observation {id} has no preservable summary or digest"),
                    });
                }
                let payload = self
                    .payload(id)
                    .map_err(fail(P::Preserve))?
                    .ok_or_else(|| E::Compaction {
                        phase: P::Preserve,
                        message: format!("observation {id} claims a payload that is not stored"),
                    })?;
                if payload.len() as u64 != o.payload_len
                    || crate::store::sha256_hex(&payload) != o.digest
                {
                    return Err(E::Compaction {
                        phase: P::Preserve,
                        message: format!(
                            "payload of {id} does not match its recorded length and digest"
                        ),
                    });
                }
                purge_bytes += o.payload_len;
            }
            let preserved_records = observations.len() as u64;
            let preserved_bytes: u64 = observations
                .iter()
                .map(|o| (o.summary.len() + o.excerpt.len() + o.digest.len()) as u64)
                .sum();
            let record = CompactionRecord {
                schema: SCHEMA_VERSION,
                session: self.session().into(),
                number,
                state: CompactionState::Preserved,
                purge: purge.clone(),
                orphans: orphans.clone(),
                purge_bytes: purge_bytes + orphan_bytes,
                preserved_records,
                preserved_bytes,
            };
            self.put(coll::COMPACTION, &number.to_string(), &record)
                .map_err(fail(P::Preserve))?;
        }

        // 3. Validate: read the record back; confirm the session reconstructs without the payloads.
        if active {
            let back: CompactionRecord = self
                .read(coll::COMPACTION, &number.to_string())
                .map_err(fail(P::Validate))?
                .ok_or_else(|| E::Compaction {
                    phase: P::Validate,
                    message: "the compaction record cannot be read back".into(),
                })?;
            // What remains to purge must be exactly a subset of what the record names.
            if purge.iter().any(|id| !back.purge.contains(id)) {
                return Err(E::Compaction {
                    phase: P::Validate,
                    message: "the compaction record disagrees with the selection".into(),
                });
            }
            report.preserved_records = back.preserved_records;
            report.preserved_bytes = back.preserved_bytes;
            self.reconstruct().map_err(|e| E::Compaction {
                phase: P::Validate,
                message: format!("recovery information is not intact: {e}"),
            })?;
        }
        if inject == Inject::AfterPreserve {
            return Err(E::Compaction {
                phase: P::Validate,
                message: "interrupted after preservation (injected)".into(),
            });
        }

        // 4. Purge, in atomic batches. A batch deletes the payloads and marks their records.
        if !orphans.is_empty() {
            let mutations: Vec<AtomicMutation> = orphans
                .iter()
                .map(|id| AtomicMutation {
                    capability: coll::PAYLOAD.into(),
                    key: key(coll::PAYLOAD, self.session(), id),
                    value: None,
                })
                .collect();
            self.db()?
                .apply_atomic_transaction(
                    &format!("{}-c{number}-orphans", self.session()),
                    None,
                    &[],
                    &mutations,
                    None,
                )
                .map_err(|e: FlowError| E::Compaction {
                    phase: P::Purge,
                    message: e.to_string(),
                })?;
        }
        const BATCH: usize = 256;
        for (n, chunk) in purge.chunks(BATCH).enumerate() {
            if let Inject::DuringPurge { batches } = inject {
                if n >= batches {
                    return Err(E::Compaction {
                        phase: P::Purge,
                        message: format!("interrupted after {n} batches (injected)"),
                    });
                }
            }
            let mut mutations = Vec::with_capacity(chunk.len() * 2);
            for id in chunk {
                let mut o = held[id.as_str()].clone();
                report.payload_bytes_purged += o.payload_len;
                report.payloads_purged += 1;
                o.payload_held = false;
                mutations.push(AtomicMutation {
                    capability: coll::PAYLOAD.into(),
                    key: key(coll::PAYLOAD, self.session(), id),
                    value: None,
                });
                mutations.push(AtomicMutation {
                    capability: coll::OBS.into(),
                    key: key(coll::OBS, self.session(), id),
                    value: Some(serde_json::to_value(&o).map_err(|e| E::Compaction {
                        phase: P::Purge,
                        message: e.to_string(),
                    })?),
                });
            }
            let tx = format!("{}-c{number}-{}", self.session(), chunk[0]);
            self.db()?
                .apply_atomic_transaction(&tx, None, &[], &mutations, None)
                .map_err(|e: FlowError| E::Compaction {
                    phase: P::Purge,
                    message: e.to_string(),
                })?;
        }

        // 5. Verify the resulting logical state.
        for id in &purge {
            let o = self.observation(id).map_err(fail(P::Verify))?;
            let gone = self.payload(id).map_err(fail(P::Verify))?.is_none();
            match o {
                Some(o) if gone && !o.payload_held && !o.summary.is_empty() => {}
                _ => {
                    return Err(E::Compaction {
                        phase: P::Verify,
                        message: format!("observation {id} is not in the expected purged state"),
                    });
                }
            }
        }
        for o in &pinned_held {
            if self.payload(&o.id).map_err(fail(P::Verify))?.is_none() {
                return Err(E::Compaction {
                    phase: P::Verify,
                    message: format!("pinned payload {} was lost", o.id),
                });
            }
        }
        self.reconstruct().map_err(|e| E::Compaction {
            phase: P::Verify,
            message: format!("recovery information is not intact after the purge: {e}"),
        })?;

        if inject == Inject::BeforeRecord {
            return Err(E::Compaction {
                phase: P::Record,
                message: "interrupted before recording the outcome (injected)".into(),
            });
        }

        // 6. Record the outcome.
        if active {
            let mut c: CompactionRecord = self
                .read(coll::COMPACTION, &number.to_string())
                .map_err(fail(P::Record))?
                .ok_or_else(|| E::Compaction {
                    phase: P::Record,
                    message: "the compaction record vanished".into(),
                })?;
            c.state = CompactionState::Purged;
            self.put(coll::COMPACTION, &number.to_string(), &c)
                .map_err(fail(P::Record))?;
        }
        if inject == Inject::BeforeReclaim {
            return Err(E::Compaction {
                phase: P::Reclaim,
                message: "interrupted before reclamation (injected)".into(),
            });
        }

        // 7. Reclaim: a separate, separately reported step.
        report.reclaim = self.reclaim().map_err(fail(P::Reclaim))?;

        // 8. Mark the session compacted, last: only once the latest compaction record says its
        //    purge finished and reclamation succeeded. A rerun after a failed reclaim lands here
        //    with nothing left to purge and still advances the counter exactly once.
        if let Some(c) = self.latest_compaction().map_err(fail(P::Record))? {
            let mut s = self.session_record().map_err(fail(P::Record))?;
            if c.state == CompactionState::Purged && s.compactions < c.number {
                s.compactions = c.number;
                self.put(coll::SESSION, "meta", &s)
                    .map_err(fail(P::Record))?;
            }
        }
        report.elapsed = started.elapsed();
        Ok(report)
    }

    fn latest_compaction(&self) -> Result<Option<CompactionRecord>> {
        let mut all: Vec<CompactionRecord> = self.list(coll::COMPACTION)?;
        all.sort_by_key(|c| c.number);
        Ok(all.pop())
    }

    /// Asks FeltDB to release what logical deletion left behind, by the public means it offers:
    ///
    /// 1. `StateStore::collect_unreachable` removes revision rows, which otherwise keep every
    ///    deleted payload readable. This session holds no refs, so *all* revision history is
    ///    collected, live records' included. Session memory has no use for revision history; a
    ///    caller who did would have to hold refs.
    /// 2. `acknowledge_peer_versions` for one nominal local peer, then `compact_operation_log`,
    ///    prune the in-memory operation log and rewrite the journal as a snapshot. FeltDB prunes
    ///    only what a configured peer has acknowledged and does nothing at all with no peers, so
    ///    this repurposes its replication API; no replication happens.
    ///
    /// Neither returns memory to the operating system by itself.
    pub fn reclaim(&mut self) -> Result<ReclaimReport> {
        let db = self.db()?.clone();
        let journal = self.journal_path();
        let size = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let mut report = ReclaimReport {
            journal_before: size(&journal),
            ..Default::default()
        };
        let store = StateStore::with_feltdb(db.clone()).map_err(|e| E::Persist(e))?;
        report.revisions_collected = store
            .collect_unreachable()
            .map_err(|e| E::Persist(format!("revision collection: {e}")))?
            .collected_revisions
            .len();
        let me = db.instance_id().map_err(|e| E::Persist(e.to_string()))?;
        let seq = db.sequence().map_err(|e| E::Persist(e.to_string()))?;
        let peer = "chip-session-memory".to_string();
        db.add_sync_peer(peer.clone())
            .map_err(|e| E::Persist(e.to_string()))?;
        db.acknowledge_peer_versions(peer.clone(), std::collections::HashMap::from([(me, seq)]))
            .map_err(|e| E::Persist(e.to_string()))?;
        report.operations_pruned = db
            .compact_operation_log(&[peer])
            .map_err(|e| E::Persist(format!("log compaction: {e}")))?;
        report.journal_after = size(&journal);
        Ok(report)
    }
}
