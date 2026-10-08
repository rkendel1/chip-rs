//! FeltDB candidate: the adapter of the original experiment, unchanged in behavior.
//!
//! Durability: `DurabilityMode` default (`Flushed`: each write is appended to the journal and
//! flushed to the operating system; not fsynced). `synced` switches to `Synced` for its writes.
//! A successful write therefore survives the death of the process, not the loss of power, until a
//! `Synced` barrier follows it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use feltdb::{AtomicMutation, DurabilityMode, FeltDb, FlowError, StateStore};
use serde_json::Value;

use super::{Backend, Mutation, file_len};
use crate::compaction::ReclaimReport;
use crate::error::{Result, SessionMemoryError as E};
use crate::schema::key;

pub struct Felt {
    pub(crate) db: Arc<FeltDb>,
    dir: std::path::PathBuf,
    session: String,
}

impl Felt {
    pub fn handle(&self) -> &Arc<FeltDb> {
        &self.db
    }
    pub fn journal_path(&self) -> std::path::PathBuf {
        self.dir.join(Self::FILE)
    }
}

fn q(e: FlowError) -> E {
    E::Query(e.to_string())
}
fn p(e: FlowError) -> E {
    E::Persist(e.to_string())
}

impl Backend for Felt {
    const NAME: &'static str = "feltdb";
    const FILE: &'static str = "session.felt";

    fn open(dir: &Path, session: &str) -> Result<Self> {
        let db = FeltDb::open(dir.join(Self::FILE)).map_err(|e| E::Init(e.to_string()))?;
        if super::sync_every_write() {
            db.set_durability_mode(DurabilityMode::Synced);
        }
        Ok(Self {
            db: Arc::new(db),
            dir: dir.to_path_buf(),
            session: session.into(),
        })
    }

    fn get(&self, collection: &str, id: &str) -> Result<Option<Value>> {
        self.db
            .get_value(&key(collection, &self.session, id))
            .map_err(q)
    }

    fn put(&self, collection: &str, id: &str, value: &Value) -> Result<()> {
        let k = key(collection, &self.session, id);
        let exists = self.db.get_value(&k).map_err(p)?.is_some();
        if exists {
            self.db.update(&k, value)
        } else {
            self.db.insert(&k, value)
        }
        .map_err(p)
    }

    fn delete(&self, collection: &str, id: &str) -> Result<()> {
        self.db
            .delete(&key(collection, &self.session, id))
            .map_err(p)
    }

    fn scan(
        &self,
        collection: &str,
        visit: &mut dyn FnMut(&str, &Value) -> Result<()>,
    ) -> Result<()> {
        let prefix = format!("{collection}:{}:", self.session);
        let mut after: Option<String> = None;
        loop {
            let page = self
                .db
                .list_collection_page(collection, after.as_deref(), 1000)
                .map_err(q)?;
            let Some(last) = page.last() else { break };
            after = Some(last.key.clone());
            for row in &page {
                let Some(id) = row.key.strip_prefix(&prefix) else {
                    return Err(E::ForeignRecord {
                        expected: self.session.clone(),
                        found: row.key.clone(),
                    });
                };
                visit(id, &row.value)?;
            }
            if page.len() < 1000 {
                break;
            }
        }
        Ok(())
    }

    fn batch(&self, tag: &str, mutations: &[Mutation]) -> Result<()> {
        let ms: Vec<AtomicMutation> = mutations
            .iter()
            .map(|m| match m {
                Mutation::Put {
                    collection,
                    id,
                    value,
                } => AtomicMutation {
                    capability: (*collection).into(),
                    key: key(collection, &self.session, id),
                    value: Some(value.clone()),
                },
                Mutation::Delete { collection, id } => AtomicMutation {
                    capability: (*collection).into(),
                    key: key(collection, &self.session, id),
                    value: None,
                },
            })
            .collect();
        self.db
            .apply_atomic_transaction(&format!("{}-{tag}", self.session), None, &[], &ms, None)
            .map_err(p)
            .map(|_| ())
    }

    fn synced(&self, writes: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        let previous = self.db.durability_mode();
        self.db.set_durability_mode(DurabilityMode::Synced);
        let r = writes();
        self.db.set_durability_mode(previous);
        r
    }

    fn position(&self) -> Result<(u64, String)> {
        Ok((
            self.db
                .sequence()
                .map_err(|e| E::Checkpoint(e.to_string()))?,
            self.db
                .state_digest()
                .map_err(|e| E::Checkpoint(e.to_string()))?,
        ))
    }

    fn cardinalities(&self) -> Result<BTreeMap<String, u64>> {
        Ok(self
            .db
            .list_cardinalities()
            .map_err(q)?
            .into_iter()
            .collect())
    }

    /// Asks FeltDB to release what logical deletion left behind, by the public means it offers:
    ///
    /// 1. `StateStore::collect_unreachable` removes revision rows, which otherwise keep every
    ///    deleted payload readable. This session holds no refs, so *all* revision history is
    ///    collected, live records' included. Session memory has no use for revision history.
    /// 2. `acknowledge_peer_versions` for one nominal local peer, then `compact_operation_log`,
    ///    prune the in-memory operation log and rewrite the journal as a snapshot. FeltDB prunes
    ///    only what a configured peer has acknowledged and does nothing at all with no peers, so
    ///    this repurposes its replication API; no replication happens.
    ///
    /// Neither returns memory to the operating system by itself.
    fn reclaim(&mut self) -> Result<ReclaimReport> {
        let db = self.db.clone();
        let mut report = ReclaimReport {
            journal_before: self.disk_bytes(),
            ..Default::default()
        };
        let store = StateStore::with_feltdb(db.clone()).map_err(E::Persist)?;
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
        report.journal_after = self.disk_bytes();
        Ok(report)
    }

    fn integrity(&mut self) -> Result<String> {
        Ok("not offered by FeltDB".into())
    }

    fn disk_bytes(&self) -> u64 {
        file_len(&self.journal_path())
    }
}
