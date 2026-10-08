//! The storage contract every candidate implements.
//!
//! Everything above this trait (the schema, retention classes, compaction, checkpoints,
//! reconstruction and reference validation) is shared code and runs identically over every
//! candidate. A backend only has to keep JSON documents addressed by `(collection, id)` for one
//! session, list a collection in id order, apply a batch atomically, run a closure behind its
//! strongest durability barrier, and give back space when asked. That is deliberately the whole
//! contract: it is what a candidate's adapter has to implement, and so what "adapter complexity"
//! means in the comparison.
//!
//! What a backend is *not* asked to provide, because a session is one writer on one store: locking
//! between sessions, multi-writer isolation, replication or query planning.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::compaction::ReclaimReport;
use crate::error::Result;

use std::sync::atomic::AtomicBool;

/// Experiment switch read by every backend's `open`: when set, each ordinary write is made as
/// durable as the engine can make it (FeltDB `Synced`, SQLite `synchronous=FULL`, redb
/// `Immediate`), instead of the default flush-to-the-operating-system class. The benchmark sets it
/// per arm; nothing else does.
pub static SYNC_EVERY_WRITE: AtomicBool = AtomicBool::new(false);

pub(crate) fn sync_every_write() -> bool {
    SYNC_EVERY_WRITE.load(std::sync::atomic::Ordering::Relaxed)
}

pub mod felt;
pub mod journal;
pub mod redb_store;
pub mod sqlite;

pub use felt::Felt;
pub use journal::Journal;
pub use redb_store::Redb;
pub use sqlite::Sqlite;

/// One write of an atomic batch.
#[derive(Debug, Clone)]
pub enum Mutation {
    Put {
        collection: &'static str,
        id: String,
        value: Value,
    },
    Delete {
        collection: &'static str,
        id: String,
    },
}

pub trait Backend: Sized + Send {
    /// Short name used in reports and file names.
    const NAME: &'static str;
    /// The main file the store keeps inside the session directory.
    const FILE: &'static str;

    /// Opens the store in `dir`, creating its files if they do not exist. Runs recovery, if the
    /// engine has any, as part of opening.
    fn open(dir: &Path, session: &str) -> Result<Self>;

    fn get(&self, collection: &str, id: &str) -> Result<Option<Value>>;

    /// Inserts or replaces. Returning `Ok` means the engine accepted the write under the
    /// engine's *default write durability* as configured by the adapter (see each adapter's
    /// module documentation); it is stronger only inside [`synced`](Self::synced).
    fn put(&self, collection: &str, id: &str, value: &Value) -> Result<()>;

    fn delete(&self, collection: &str, id: &str) -> Result<()>;

    /// Visits every record of a collection in ascending id order. The visitor may fail, which
    /// stops the scan with that error.
    fn scan(
        &self,
        collection: &str,
        visit: &mut dyn FnMut(&str, &Value) -> Result<()>,
    ) -> Result<()>;

    /// All-or-nothing: after a crash either every mutation of `mutations` is visible or none is.
    fn batch(&self, tag: &str, mutations: &[Mutation]) -> Result<()>;

    /// Runs `writes` with the strongest durability barrier the engine offers, so that when it
    /// returns `Ok` everything written before and during it is on stable storage as far as the
    /// engine can promise. Restores the default afterwards.
    fn synced(&self, writes: &mut dyn FnMut() -> Result<()>) -> Result<()>;

    /// `(sequence, digest)` recorded in a checkpoint. Engine-defined and **not comparable across
    /// engines**; see the report. Nothing in the recovery packet depends on it.
    fn position(&self) -> Result<(u64, String)>;

    /// Rows per collection as the *engine* counts them (FeltDB includes revision rows).
    fn cardinalities(&self) -> Result<BTreeMap<String, u64>>;

    /// Gives back what logical deletion left behind, by the means the engine offers.
    fn reclaim(&mut self) -> Result<ReclaimReport>;

    /// The engine's own integrity check, if it has one: `Ok("ok")` or a description of damage.
    fn integrity(&mut self) -> Result<String>;

    /// Bytes the store occupies on disk: main file plus journal or write-ahead log.
    fn disk_bytes(&self) -> u64;
}

pub(crate) fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}
