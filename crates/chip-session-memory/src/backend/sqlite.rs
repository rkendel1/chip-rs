//! SQLite candidate (`rusqlite`, bundled SQLite).
//!
//! Layout: one table `records(coll, id, body)`, `body` being the same JSON document the other
//! candidates store, with a unique index on `(coll, id)`. A rowid table rather than `WITHOUT
//! ROWID`, because payload rows are up to 128 KiB and SQLite recommends rowid tables for large
//! rows.
//!
//! Durability configuration (what a successful write guarantees):
//!
//! * `journal_mode=WAL`, `synchronous=NORMAL`. Every `put`/`delete` is its own transaction. A
//!   committed transaction is appended to the write-ahead log and is visible to any later opener
//!   even if this process is killed, because the log is in the operating system's file cache. It
//!   is **not** fsynced at commit, so a power loss or kernel crash may lose the most recent
//!   commits (the database stays consistent: SQLite's documented guarantee for WAL+NORMAL).
//! * With `SYNC_EVERY_WRITE` set, the baseline is `synchronous=FULL` (fsync at every commit).
//! * [`Backend::synced`] runs its writes with `synchronous=FULL`, which fsyncs the WAL at each
//!   commit; the fsync covers every frame already in the log, so everything before it is durable
//!   too.
//! * A batch is one transaction.
//! * `cache_size` is bounded (16 MiB) so memory is not traded for speed unnoticed; SQLite's own
//!   default is about 2 MiB. Memory-mapped I/O is off (SQLite's default).
//!
//! Reclamation: `wal_checkpoint(TRUNCATE)` then `VACUUM` rewrites the database file without free
//! pages (it needs temporary space up to the size of the database), then the WAL is truncated.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Backend, Mutation, file_len};
use crate::compaction::ReclaimReport;
use crate::error::{Result, SessionMemoryError as E};

pub const CACHE_KIB: i64 = 16 * 1024;

pub struct Sqlite {
    conn: Connection,
    path: PathBuf,
    baseline: &'static str,
}

fn q(e: impl std::fmt::Display) -> E {
    E::Query(e.to_string())
}
fn p(e: impl std::fmt::Display) -> E {
    E::Persist(e.to_string())
}

impl Sqlite {
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
    fn wal(&self) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push("-wal");
        PathBuf::from(s)
    }
    fn shm(&self) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push("-shm");
        PathBuf::from(s)
    }
    fn parse(body: String) -> Result<Value> {
        serde_json::from_str(&body).map_err(q)
    }
}

impl Backend for Sqlite {
    const NAME: &'static str = "sqlite";
    const FILE: &'static str = "session.sqlite";

    fn open(dir: &Path, _session: &str) -> Result<Self> {
        let path = dir.join(Self::FILE);
        let init = |e: rusqlite::Error| E::Init(e.to_string());
        let conn = Connection::open(&path).map_err(init)?;
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(init)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(E::Init(format!("could not enable WAL, mode is {mode}")));
        }
        let sync = if super::sync_every_write() {
            "FULL"
        } else {
            "NORMAL"
        };
        conn.execute_batch(&format!(
            "PRAGMA synchronous={sync};
             PRAGMA cache_size=-{CACHE_KIB};
             CREATE TABLE IF NOT EXISTS records(
                 coll TEXT NOT NULL, id TEXT NOT NULL, body TEXT NOT NULL,
                 UNIQUE(coll, id));"
        ))
        .map_err(init)?;
        Ok(Self {
            conn,
            path,
            baseline: sync,
        })
    }

    fn get(&self, collection: &str, id: &str) -> Result<Option<Value>> {
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM records WHERE coll=?1 AND id=?2")
            .and_then(|mut s| {
                s.query_row(params![collection, id], |r| r.get(0))
                    .optional()
            })
            .map_err(q)?;
        body.map(Self::parse).transpose()
    }

    fn put(&self, collection: &str, id: &str, value: &Value) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO records(coll,id,body) VALUES(?1,?2,?3)
                 ON CONFLICT(coll,id) DO UPDATE SET body=excluded.body",
            )
            .and_then(|mut s| s.execute(params![collection, id, value.to_string()]))
            .map_err(p)
            .map(|_| ())
    }

    fn delete(&self, collection: &str, id: &str) -> Result<()> {
        self.conn
            .prepare_cached("DELETE FROM records WHERE coll=?1 AND id=?2")
            .and_then(|mut s| s.execute(params![collection, id]))
            .map_err(p)
            .map(|_| ())
    }

    fn scan(
        &self,
        collection: &str,
        visit: &mut dyn FnMut(&str, &Value) -> Result<()>,
    ) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, body FROM records WHERE coll=?1 ORDER BY id")
            .map_err(q)?;
        let mut rows = stmt.query(params![collection]).map_err(q)?;
        while let Some(row) = rows.next().map_err(q)? {
            let id: String = row.get(0).map_err(q)?;
            let body: String = row.get(1).map_err(q)?;
            visit(&id, &Self::parse(body)?)?;
        }
        Ok(())
    }

    fn batch(&self, _tag: &str, mutations: &[Mutation]) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(p)?;
        for m in mutations {
            match m {
                Mutation::Put {
                    collection,
                    id,
                    value,
                } => self.put(collection, id, value)?,
                Mutation::Delete { collection, id } => self.delete(collection, id)?,
            }
        }
        tx.commit().map_err(p)
    }

    fn synced(&self, writes: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA synchronous=FULL")
            .map_err(p)?;
        let r = writes();
        let restored = self
            .conn
            .execute_batch(&format!("PRAGMA synchronous={}", self.baseline))
            .map_err(p);
        r.and(restored)
    }

    fn position(&self) -> Result<(u64, String)> {
        let rows: i64 = self
            .conn
            .query_row("SELECT count(*) FROM records", [], |r| r.get(0))
            .map_err(|e| E::Checkpoint(e.to_string()))?;
        // Digest of every record except payload bodies (payloads contribute id and length), so a
        // checkpoint does not hash hundreds of megabytes. A different definition from FeltDB's.
        let mut h = Sha256::new();
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT coll, id, CASE WHEN coll='payload' THEN length(body) ELSE body END
                 FROM records ORDER BY coll, id",
            )
            .map_err(|e| E::Checkpoint(e.to_string()))?;
        let mut rows_it = stmt.query([]).map_err(|e| E::Checkpoint(e.to_string()))?;
        while let Some(r) = rows_it.next().map_err(|e| E::Checkpoint(e.to_string()))? {
            let (c, i): (String, String) =
                (r.get(0).unwrap_or_default(), r.get(1).unwrap_or_default());
            let v: rusqlite::types::Value = r.get(2).map_err(|e| E::Checkpoint(e.to_string()))?;
            h.update(c);
            h.update([0]);
            h.update(i);
            h.update([0]);
            match v {
                rusqlite::types::Value::Text(t) => h.update(t),
                rusqlite::types::Value::Integer(n) => h.update(n.to_string()),
                _ => {}
            }
            h.update([1]);
        }
        let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Ok((rows as u64, digest))
    }

    fn cardinalities(&self) -> Result<BTreeMap<String, u64>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT coll, count(*) FROM records GROUP BY coll")
            .map_err(q)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })
            .map_err(q)?;
        rows.collect::<std::result::Result<_, _>>().map_err(q)
    }

    fn reclaim(&mut self) -> Result<ReclaimReport> {
        let mut report = ReclaimReport {
            journal_before: self.disk_bytes(),
            ..Default::default()
        };
        let run = |c: &Connection, sql: &str| {
            c.query_row(sql, [], |_| Ok(())).or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(()),
                other => Err(other),
            })
        };
        run(&self.conn, "PRAGMA wal_checkpoint(TRUNCATE)").map_err(p)?;
        self.conn.execute_batch("VACUUM").map_err(p)?;
        run(&self.conn, "PRAGMA wal_checkpoint(TRUNCATE)").map_err(p)?;
        report.journal_after = self.disk_bytes();
        Ok(report)
    }

    fn integrity(&mut self) -> Result<String> {
        self.conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(q)
    }

    fn disk_bytes(&self) -> u64 {
        file_len(&self.path) + file_len(&self.wal()) + file_len(&self.shm())
    }
}
