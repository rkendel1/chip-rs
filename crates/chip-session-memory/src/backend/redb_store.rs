//! redb candidate (pure-Rust embedded copy-on-write B-tree, ACID, single process).
//!
//! Layout: one table keyed by `(collection, id)` whose value is the same JSON document text the
//! other candidates store.
//!
//! Durability configuration (what a successful write guarantees):
//!
//! * Each `put`/`delete` is its own write transaction committed with `Durability::None`: it is
//!   visible to later readers in this process, but redb states it is *not persisted* unless a
//!   later `Durability::Immediate` commit follows. Whether such a commit survives the death of the
//!   process is therefore not promised by redb and is measured by the kill tests rather than
//!   assumed. This is the closest redb setting to the flush-without-fsync default of the other
//!   candidates; it is not the same guarantee, and the report says so.
//! * With `SYNC_EVERY_WRITE` set, every commit is `Immediate`.
//! * [`Backend::synced`] runs its writes in transactions committed with `Durability::Immediate`
//!   (fsynced before `commit` returns), which also makes every earlier `None` commit durable.
//! * A batch is one write transaction.
//! * The page cache is bounded (16 MiB) by default in this adapter; redb's own default is 1 GiB
//!   and is exercised by a separate `redb_default` benchmark arm.
//!
//! Reclamation: redb reuses freed pages but does not shrink the file by itself; `Database::compact`
//! rewrites it. It needs `&mut Database`, which is why [`Backend::reclaim`] takes `&mut self`.
//!
//! Opening a database that was not closed cleanly makes redb repair it (a scan whose cost grows
//! with the database), which shows up in the recovery latency.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use redb::{
    Builder, Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    TableDefinition,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Backend, Mutation, file_len};
use crate::compaction::ReclaimReport;
use crate::error::{Result, SessionMemoryError as E};

const RECORDS: TableDefinition<(&str, &str), &str> = TableDefinition::new("records");

pub const CACHE_BYTES: usize = 16 * 1024 * 1024;

/// redb's own default page cache (1 GiB), for the benchmark arm that measures a naive adoption.
pub const REDB_DEFAULT_CACHE_BYTES: usize = 1024 * 1024 * 1024;

/// Cache size `open` uses. The adapter's bounded 16 MiB unless a benchmark arm overrides it.
pub static CACHE_BYTES_OVERRIDE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(CACHE_BYTES);

pub struct Redb {
    db: Database,
    path: PathBuf,
    /// When set, every commit is `Immediate`.
    immediate: std::cell::Cell<bool>,
    cache_bytes: usize,
}

fn q(e: impl std::fmt::Display) -> E {
    E::Query(e.to_string())
}
fn p(e: impl std::fmt::Display) -> E {
    E::Persist(e.to_string())
}

impl Redb {
    /// Opens with an explicit cache size, for the default-configuration benchmark arm.
    pub fn open_with_cache(dir: &Path, cache_bytes: usize) -> Result<Self> {
        let path = dir.join(Self::FILE);
        let mut b = Builder::new();
        b.set_cache_size(cache_bytes);
        let db = b.create(&path).map_err(|e| E::Init(e.to_string()))?;
        {
            let w = db.begin_write().map_err(|e| E::Init(e.to_string()))?;
            w.open_table(RECORDS).map_err(|e| E::Init(e.to_string()))?;
            w.commit().map_err(|e| E::Init(e.to_string()))?;
        }
        Ok(Self {
            db,
            path,
            immediate: std::cell::Cell::new(super::sync_every_write()),
            cache_bytes,
        })
    }

    pub fn cache_bytes(&self) -> usize {
        self.cache_bytes
    }

    fn txn(&self) -> Result<redb::WriteTransaction> {
        let mut w = self.db.begin_write().map_err(p)?;
        w.set_durability(if self.immediate.get() {
            Durability::Immediate
        } else {
            Durability::None
        })
        .map_err(p)?;
        Ok(w)
    }
}

// SAFETY-free note: `Cell` makes the type !Sync, which is fine: a session is single-writer and
// `Backend` only requires `Send`.

impl Backend for Redb {
    const NAME: &'static str = "redb";
    const FILE: &'static str = "session.redb";

    fn open(dir: &Path, _session: &str) -> Result<Self> {
        Self::open_with_cache(
            dir,
            CACHE_BYTES_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    fn get(&self, collection: &str, id: &str) -> Result<Option<Value>> {
        let r = self.db.begin_read().map_err(q)?;
        let t = r.open_table(RECORDS).map_err(q)?;
        match t.get((collection, id)).map_err(q)? {
            Some(v) => Ok(Some(serde_json::from_str(v.value()).map_err(q)?)),
            None => Ok(None),
        }
    }

    fn put(&self, collection: &str, id: &str, value: &Value) -> Result<()> {
        let w = self.txn()?;
        {
            let mut t = w.open_table(RECORDS).map_err(p)?;
            t.insert((collection, id), value.to_string().as_str())
                .map_err(p)?;
        }
        w.commit().map_err(p)
    }

    fn delete(&self, collection: &str, id: &str) -> Result<()> {
        let w = self.txn()?;
        {
            let mut t = w.open_table(RECORDS).map_err(p)?;
            t.remove((collection, id)).map_err(p)?;
        }
        w.commit().map_err(p)
    }

    fn scan(
        &self,
        collection: &str,
        visit: &mut dyn FnMut(&str, &Value) -> Result<()>,
    ) -> Result<()> {
        let r = self.db.begin_read().map_err(q)?;
        let t = r.open_table(RECORDS).map_err(q)?;
        for row in t.range((collection, "")..).map_err(q)? {
            let (k, v) = row.map_err(q)?;
            let (c, id) = k.value();
            if c != collection {
                break;
            }
            let value: Value = serde_json::from_str(v.value()).map_err(q)?;
            visit(id, &value)?;
        }
        Ok(())
    }

    fn batch(&self, _tag: &str, mutations: &[Mutation]) -> Result<()> {
        let w = self.txn()?;
        {
            let mut t = w.open_table(RECORDS).map_err(p)?;
            for m in mutations {
                match m {
                    Mutation::Put {
                        collection,
                        id,
                        value,
                    } => {
                        t.insert((*collection, id.as_str()), value.to_string().as_str())
                            .map_err(p)?;
                    }
                    Mutation::Delete { collection, id } => {
                        t.remove((*collection, id.as_str())).map_err(p)?;
                    }
                }
            }
        }
        w.commit().map_err(p)
    }

    fn synced(&self, writes: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        let previous = self.immediate.replace(true);
        let r = writes();
        self.immediate.set(previous);
        r
    }

    fn position(&self) -> Result<(u64, String)> {
        let bad = |e: &dyn std::fmt::Display| E::Checkpoint(e.to_string());
        let r = self.db.begin_read().map_err(|e| bad(&e))?;
        let t = r.open_table(RECORDS).map_err(|e| bad(&e))?;
        let rows = t.len().map_err(|e| bad(&e))?;
        let mut h = Sha256::new();
        for row in t.iter().map_err(|e| bad(&e))? {
            let (k, v) = row.map_err(|e| bad(&e))?;
            let (c, id) = k.value();
            h.update(c);
            h.update([0]);
            h.update(id);
            h.update([0]);
            if c == "payload" {
                h.update(v.value().len().to_string());
            } else {
                h.update(v.value());
            }
            h.update([1]);
        }
        let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Ok((rows, digest))
    }

    fn cardinalities(&self) -> Result<BTreeMap<String, u64>> {
        let r = self.db.begin_read().map_err(q)?;
        let t = r.open_table(RECORDS).map_err(q)?;
        let mut out = BTreeMap::new();
        for row in t.iter().map_err(q)? {
            let (k, _) = row.map_err(q)?;
            *out.entry(k.value().0.to_string()).or_insert(0) += 1;
        }
        Ok(out)
    }

    fn reclaim(&mut self) -> Result<ReclaimReport> {
        let mut report = ReclaimReport {
            journal_before: self.disk_bytes(),
            ..Default::default()
        };
        // Make everything committed so far durable, then rewrite the file without free pages.
        {
            let mut w = self.db.begin_write().map_err(p)?;
            w.set_durability(Durability::Immediate).map_err(p)?;
            w.commit().map_err(p)?;
        }
        self.db.compact().map_err(p)?;
        report.journal_after = self.disk_bytes();
        Ok(report)
    }

    fn integrity(&mut self) -> Result<String> {
        match self.db.check_integrity().map_err(q)? {
            true => Ok("ok".into()),
            false => Ok("repaired: the database was damaged and redb repaired it".into()),
        }
    }

    fn disk_bytes(&self) -> u64 {
        file_len(&self.path)
    }
}
