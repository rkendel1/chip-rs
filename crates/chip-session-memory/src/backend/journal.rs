//! Journal candidate: a small, single-writer recovery journal built on the `durability` crate's
//! primitives, not a database.
//!
//! What the `durability` crate provides here (raw-bytes API only, `default-features = false`):
//! a segmented write-ahead log with CRC-framed entries and torn-tail repair, CRC-validated
//! checkpoint files published by atomic rename, and WAL prefix truncation. What this adapter adds
//! (the part Chip would own): the entry format, replay into an in-memory map, payload files, the
//! checkpoint protocol and the reconciliation after a crash.
//!
//! Layout inside the session directory:
//!
//! * `wal/` segments: one entry per logical write of a *small* record (`Put`, `Delete`, or a
//!   `Batch` of them, which is the atomic unit: one entry is all-or-nothing, a torn one is dropped
//!   by the crate's tail repair). Payloads are **not** in the log.
//! * `ckpt-<last entry id>.bin`: the whole in-memory map at that point, CRC-checked.
//! * `payloads/<id>.json`: one file per transient payload, written to a temporary name and renamed
//!   so a reader never sees a partial file. Payload bodies are never resident in memory; the
//!   observation record that describes a payload carries its SHA-256, so a damaged payload is
//!   detectable by the adapter's own digest check.
//!
//! Recovery: load the newest checkpoint (refuse if it is corrupt or if the log no longer reaches
//! back to it), replay later log entries (stopping at a torn final entry), reconcile.
//!
//! Durability configuration (what a successful write guarantees):
//!
//! * Log entries are flushed to the operating system at every append (an explicit `flush()` after
//!   each one; see `append`): they survive the death of the process, not the loss of power.
//!   Payload files are written and renamed without fsync.
//! * [`Backend::synced`] fsyncs the log segment and its directory, every payload file written since
//!   the last barrier, and the payload directory. With `SYNC_EVERY_WRITE` the same is done for every
//!   write.
//! * A checkpoint file is always fsynced (the crate's `atomic_write` does it) and the log is
//!   fsynced before any prefix is deleted.
//!
//! Known limits, stated rather than hidden: the crate's single-writer guard is a lock *file* that
//! a crash leaves behind, so recovery removes it (`resume_after_crash`) and nothing here stops a
//! second live process; there is no multi-process locking. A failed fsync poisons the log writer
//! and the session must be reopened.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use durability::checkpoint::CheckpointFile;
use durability::storage::{Directory, FlushPolicy, FsDirectory};
use durability::walog::{WalMaintenance, WalReader, WalWriter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Backend, Mutation, file_len};
use crate::compaction::ReclaimReport;
use crate::error::{Result, SessionMemoryError as E};

const PAYLOAD: &str = "payload";
const SEGMENT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
enum Op {
    Put {
        c: String,
        i: String,
        v: Value,
    },
    Del {
        c: String,
        i: String,
    },
    Batch {
        ops: Vec<Op>,
    },
    /// A checkpoint was published; carries no state of its own.
    Ckpt {
        last: u64,
    },
}

type Map = BTreeMap<String, BTreeMap<String, Value>>;

fn apply(map: &mut Map, op: &Op) {
    match op {
        Op::Put { c, i, v } => {
            map.entry(c.clone())
                .or_default()
                .insert(i.clone(), v.clone());
        }
        Op::Del { c, i } => {
            if let Some(m) = map.get_mut(c) {
                m.remove(i);
            }
        }
        Op::Batch { ops } => ops.iter().for_each(|o| apply(map, o)),
        Op::Ckpt { .. } => {}
    }
}

fn p(e: impl std::fmt::Display) -> E {
    E::Persist(e.to_string())
}
fn q(e: impl std::fmt::Display) -> E {
    E::Query(e.to_string())
}

pub struct Journal {
    root: PathBuf,
    dir: Arc<dyn Directory>,
    wal: RefCell<WalWriter<()>>,
    map: RefCell<Map>,
    /// Payload files written since the last stable-storage barrier.
    unsynced: RefCell<Vec<PathBuf>>,
    force_sync: Cell<bool>,
}

impl Journal {
    fn payload_dir(&self) -> PathBuf {
        self.root.join("payloads")
    }

    fn payload_path(&self, id: &str) -> PathBuf {
        self.payload_dir().join(format!("{id}.json"))
    }

    fn sync_log(&self) -> Result<()> {
        self.wal.borrow_mut().flush_and_sync().map_err(p)
    }

    fn syncing(&self) -> bool {
        self.force_sync.get() || super::sync_every_write()
    }

    fn fsync_path(path: &Path) -> Result<()> {
        std::fs::File::open(path)
            .and_then(|f| f.sync_all())
            .map_err(p)
    }

    fn write_payload(&self, id: &str, value: &Value) -> Result<()> {
        let path = self.payload_path(id);
        let tmp = path.with_extension("json.tmp");
        let mut f = std::fs::File::create(&tmp).map_err(p)?;
        f.write_all(value.to_string().as_bytes()).map_err(p)?;
        if self.syncing() {
            f.sync_all().map_err(p)?;
        }
        drop(f);
        std::fs::rename(&tmp, &path).map_err(p)?;
        if self.syncing() {
            Self::fsync_path(&self.payload_dir())?;
        } else {
            self.unsynced.borrow_mut().push(path);
        }
        Ok(())
    }

    fn remove_payload(&self, id: &str) -> Result<()> {
        match std::fs::remove_file(self.payload_path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(p(e)),
        }
    }

    fn append(&self, op: &Op) -> Result<u64> {
        let bytes = serde_json::to_vec(op).map_err(p)?;
        let mut wal = self.wal.borrow_mut();
        let id = wal.append_bytes(&bytes).map_err(p)?;
        // Explicit, because a writer resumed after a restart is hard-wired by the crate to flush
        // every 64 appends through a 64 KiB buffer, with no setter, and dropping a writer does not
        // flush. Without this the "flushed at every append" guarantee holds only for a fresh log.
        wal.flush().map_err(p)?;
        if self.syncing() {
            wal.flush_and_sync().map_err(p)?;
        }
        Ok(id)
    }

    fn checkpoint_names(dir: &Arc<dyn Directory>) -> Result<Vec<String>> {
        let mut names: Vec<String> = dir
            .list_dir("")
            .map_err(|e| E::Init(e.to_string()))?
            .into_iter()
            .filter(|n| n.starts_with("ckpt-") && n.ends_with(".bin"))
            .collect();
        names.sort();
        Ok(names)
    }

    fn payload_names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(self.payload_dir()).map_err(q)? {
            let name = entry.map_err(q)?.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".json") {
                names.push(id.to_string());
            }
        }
        names.sort();
        Ok(names)
    }
}

impl Backend for Journal {
    const NAME: &'static str = "journal";
    const FILE: &'static str = "journal.marker";

    fn open(dir: &Path, _session: &str) -> Result<Self> {
        let init = |e: &dyn std::fmt::Display| E::Init(e.to_string());
        std::fs::create_dir_all(dir.join("payloads")).map_err(|e| init(&e))?;
        let marker = dir.join(Self::FILE);
        const MARKER: &[u8] = b"chip session journal v1\n";
        if !marker.exists() {
            std::fs::write(&marker, MARKER).map_err(|e| init(&e))?;
        } else if std::fs::read(&marker).map_err(|e| init(&e))? != MARKER {
            return Err(init(&"the journal marker is not this format and version"));
        }
        // Temporary files an interrupted write left behind are not data.
        for d in [dir.to_path_buf(), dir.join("payloads")] {
            for entry in std::fs::read_dir(&d).map_err(|e| init(&e))?.flatten() {
                if entry.file_name().to_string_lossy().ends_with(".tmp") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let fsdir: Arc<dyn Directory> = FsDirectory::arc(dir).map_err(|e| init(&e))?;

        // 1. The newest checkpoint, if any. A corrupt newest checkpoint is refused, not skipped:
        //    the log it replaced is gone.
        let mut map = Map::new();
        let mut last = 0u64;
        if let Some(name) = Self::checkpoint_names(&fsdir)?.pop() {
            let (id, bytes) = CheckpointFile::new(fsdir.clone())
                .read_bytes(&name)
                .map_err(|e| init(&format!("checkpoint {name}: {e}")))?;
            map = serde_json::from_slice(&bytes)
                .map_err(|e| init(&format!("checkpoint {name}: {e}")))?;
            last = id;
        }

        // 2. Replay the log after it. A torn final entry is dropped by the crate; damage anywhere
        //    else is an error.
        let has_wal = fsdir.exists("wal");
        let mut expected = last + 1;
        if has_wal {
            let records = WalReader::<()>::new(fsdir.clone())
                .replay_bytes_best_effort()
                .map_err(|e| init(&format!("write-ahead log: {e}")))?;
            for rec in records.into_iter().filter(|r| r.entry_id > last) {
                if rec.entry_id != expected {
                    return Err(init(&format!(
                        "the log skips from entry {} to {}: history is missing",
                        expected - 1,
                        rec.entry_id
                    )));
                }
                let op: Op = serde_json::from_slice(&rec.payload)
                    .map_err(|e| init(&format!("log entry {}: {e}", rec.entry_id)))?;
                apply(&mut map, &op);
                expected += 1;
            }
        }

        // 3. Resume appending. The crate's lock is a file a crash leaves behind, so it is cleared
        //    here; single-writer is the contract, not something the crate enforces.
        let mut wal = if has_wal {
            WalWriter::<()>::resume_after_crash(fsdir.clone()).map_err(|e| init(&e))?
        } else {
            WalWriter::<()>::with_options(fsdir.clone(), FlushPolicy::PerAppend, 0)
        };
        wal.set_segment_size_limit_bytes(SEGMENT_BYTES);
        Ok(Self {
            root: dir.to_path_buf(),
            dir: fsdir,
            wal: RefCell::new(wal),
            map: RefCell::new(map),
            unsynced: RefCell::new(Vec::new()),
            force_sync: Cell::new(false),
        })
    }

    fn get(&self, collection: &str, id: &str) -> Result<Option<Value>> {
        if collection == PAYLOAD {
            return match std::fs::read(self.payload_path(id)) {
                Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(q)?)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(q(e)),
            };
        }
        Ok(self
            .map
            .borrow()
            .get(collection)
            .and_then(|m| m.get(id))
            .cloned())
    }

    fn put(&self, collection: &str, id: &str, value: &Value) -> Result<()> {
        if collection == PAYLOAD {
            return self.write_payload(id, value);
        }
        let op = Op::Put {
            c: collection.into(),
            i: id.into(),
            v: value.clone(),
        };
        self.append(&op)?;
        apply(&mut self.map.borrow_mut(), &op);
        Ok(())
    }

    fn delete(&self, collection: &str, id: &str) -> Result<()> {
        if collection == PAYLOAD {
            return self.remove_payload(id);
        }
        let op = Op::Del {
            c: collection.into(),
            i: id.into(),
        };
        self.append(&op)?;
        apply(&mut self.map.borrow_mut(), &op);
        Ok(())
    }

    fn scan(
        &self,
        collection: &str,
        visit: &mut dyn FnMut(&str, &Value) -> Result<()>,
    ) -> Result<()> {
        if collection == PAYLOAD {
            for id in self.payload_names()? {
                if let Some(v) = self.get(PAYLOAD, &id)? {
                    visit(&id, &v)?;
                }
            }
            return Ok(());
        }
        // Snapshot the rows so the visitor may write.
        let rows: Vec<(String, Value)> = self
            .map
            .borrow()
            .get(collection)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        for (id, v) in &rows {
            visit(id, v)?;
        }
        Ok(())
    }

    fn batch(&self, _tag: &str, mutations: &[Mutation]) -> Result<()> {
        // Order matters and is the whole crash story: payloads that are added exist before the
        // record that names them; the records change in one atomic log entry; payloads that are
        // removed go last. A crash between steps leaves an unreferenced payload file (an orphan
        // the adapter purges), never a record that names a missing payload.
        let mut ops = Vec::new();
        let mut removals = Vec::new();
        for m in mutations {
            match m {
                Mutation::Put {
                    collection,
                    id,
                    value,
                } if *collection == PAYLOAD => self.write_payload(id, value)?,
                Mutation::Delete { collection, id } if *collection == PAYLOAD => {
                    removals.push(id.clone())
                }
                Mutation::Put {
                    collection,
                    id,
                    value,
                } => ops.push(Op::Put {
                    c: (*collection).into(),
                    i: id.clone(),
                    v: value.clone(),
                }),
                Mutation::Delete { collection, id } => ops.push(Op::Del {
                    c: (*collection).into(),
                    i: id.clone(),
                }),
            }
        }
        if !ops.is_empty() {
            let op = Op::Batch { ops };
            self.append(&op)?;
            apply(&mut self.map.borrow_mut(), &op);
        }
        for id in removals {
            self.remove_payload(&id)?;
        }
        if self.syncing() {
            Self::fsync_path(&self.payload_dir())?;
        }
        Ok(())
    }

    fn synced(&self, writes: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        // Barrier for everything written so far: payload files first (a record must not be
        // durable before the payload it names), then the log.
        for path in self.unsynced.borrow_mut().drain(..) {
            if path.exists() {
                Self::fsync_path(&path)?;
            }
        }
        Self::fsync_path(&self.payload_dir())?;
        let previous = self.force_sync.replace(true);
        let r = writes();
        self.force_sync.set(previous);
        r?;
        self.sync_log()
    }

    fn position(&self) -> Result<(u64, String)> {
        let seq = self.wal.borrow().last_entry_id().unwrap_or(0);
        let mut h = Sha256::new();
        for (c, rows) in self.map.borrow().iter() {
            for (i, v) in rows {
                h.update(c);
                h.update([0]);
                h.update(i);
                h.update([0]);
                h.update(v.to_string());
                h.update([1]);
            }
        }
        let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Ok((seq, digest))
    }

    fn cardinalities(&self) -> Result<BTreeMap<String, u64>> {
        let mut out: BTreeMap<String, u64> = self
            .map
            .borrow()
            .iter()
            .filter(|(_, m)| !m.is_empty())
            .map(|(c, m)| (c.clone(), m.len() as u64))
            .collect();
        let payloads = self.payload_names()?.len() as u64;
        if payloads > 0 {
            out.insert(PAYLOAD.into(), payloads);
        }
        Ok(out)
    }

    /// Publishes a checkpoint of the in-memory map and deletes the log it covers: checkpoint file
    /// durable first, then a log entry recording it made durable, and only then the prefix and the
    /// older checkpoints are deleted (the order the crate's own publish helper uses).
    fn reclaim(&mut self) -> Result<ReclaimReport> {
        let mut report = ReclaimReport {
            journal_before: self.disk_bytes(),
            ..Default::default()
        };
        let last = self.wal.borrow().last_entry_id().unwrap_or(0);
        if last > 0 {
            let name = format!("ckpt-{last:020}.bin");
            let bytes = serde_json::to_vec(&*self.map.borrow()).map_err(p)?;
            CheckpointFile::new(self.dir.clone())
                .write_bytes_durable(&name, last, &bytes)
                .map_err(p)?;
            {
                let mut wal = self.wal.borrow_mut();
                wal.append_bytes(&serde_json::to_vec(&Op::Ckpt { last }).map_err(p)?)
                    .map_err(p)?;
                wal.flush_and_sync().map_err(p)?;
            }
            report.operations_pruned = WalMaintenance::new(self.dir.clone())
                .truncate_prefix(last)
                .map_err(p)?;
            for old in Self::checkpoint_names(&self.dir)? {
                if old != name {
                    self.dir.delete(&old).map_err(p)?;
                }
            }
        }
        report.journal_after = self.disk_bytes();
        Ok(report)
    }

    fn integrity(&mut self) -> Result<String> {
        let mut problems = Vec::new();
        if let Err(e) = WalMaintenance::new(self.dir.clone()).segment_ranges_strict() {
            problems.push(format!("log: {e}"));
        }
        for name in Self::checkpoint_names(&self.dir)? {
            if let Err(e) = CheckpointFile::new(self.dir.clone()).read_bytes(&name) {
                problems.push(format!("{name}: {e}"));
            }
        }
        for id in self.payload_names()? {
            if let Err(e) = self.get(PAYLOAD, &id) {
                problems.push(format!("payload {id}: {e}"));
            }
        }
        Ok(if problems.is_empty() {
            "ok".into()
        } else {
            format!("damaged: {}", problems.join("; "))
        })
    }

    fn disk_bytes(&self) -> u64 {
        fn walk(p: &Path) -> u64 {
            std::fs::read_dir(p)
                .map(|d| {
                    d.flatten()
                        .map(|e| match e.metadata() {
                            Ok(m) if m.is_dir() => walk(&e.path()),
                            Ok(m) => m.len(),
                            Err(_) => 0,
                        })
                        .sum()
                })
                .unwrap_or(0)
        }
        walk(&self.root).max(file_len(&self.root.join(Self::FILE)))
    }
}
