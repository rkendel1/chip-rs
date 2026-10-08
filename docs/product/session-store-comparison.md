# Session store: requirements first, then what meets them

Status: **assessment**. No production crate depends on any storage engine measured here, Chip's
runtime, session identity, work lifecycle and escalation behavior are unchanged, and FeltDB's role
as the platform's durable-state authority is untouched. Everything lives in the leaf experiment
crate `crates/chip-session-memory`. Nothing here claims anything about model quality, task
completion or agent intelligence.

## 1. What problem is real today

Two different things were being discussed as one:

| | Status | Evidence |
|---|---|---|
| **Memory retention of finished work** | **Real, measured, today.** | §2 |
| Recovering or resuming work after a restart | Hypothetical. Chip has no session concept, no resume and no checkpoint format. | `Agent::run_work_with_context_policy` returns a `WorkReport` and ends; `Service.items` is an in-process map |
| A general-purpose database, multiple writers or processes, replication | Not a requirement. | none |

Persistence is also not resumability. A store can give back bytes and records; Chip would still have
to know which action was in flight, which already happened, what is safe to retry and what needs
escalation. That policy belongs in Chip's work lifecycle, not in a storage engine, and this PR does
not design it.

### The minimum contract (if resume ever becomes a requirement)

A small, local, **single-writer recovery journal**, not a database:

| Requirement | Minimum contract | Tested by |
|---|---|---|
| Session identity | Stable id for a work run; every record carries it; a record naming another session is refused | `sessions_are_isolated…`, `missing_corrupt_and_inconsistent…` |
| Append | Observations, attempts, test results, repairs, hypotheses, escalations, checkpoints, pending decisions | shared adapter |
| Recovery | Reopen after restart; reconstruct the last valid state | `normal_close_and_reopen…`, kill tests |
| Integrity | Refuse inconsistent or missing references by name; do not invent state from a torn final write | `missing_corrupt_and_inconsistent…`, `a_damaged_storage_file…`, bit-flip probe |
| Durability | Say exactly what a successful write guarantees (§5) | per-engine configuration |
| Checkpointing | A barrier after which everything before it survives a process kill (and, with fsync, power loss) | `…killed_after_a_committed_checkpoint…` |
| Compaction | Drop eligible transient payloads, keep unresolved failures' diagnostics and open hypotheses' evidence; interruptible at every step | `compaction_interrupted_at_every_seam…`, `…killed_during_compaction…` |
| Idempotency | Recovering or re-running compaction repeatedly creates no duplicate logical records | `repeated_recovery_neither_duplicates…` |
| Memory | Do not hold every historical payload in RAM because it is persisted | §4 |
| Isolation | No production dependency | `scripts/audit-dependencies.sh` unchanged and passing |

Explicitly **not** required: SQL or any query language, concurrent writers or processes,
replication, a universal document store, background compaction threads, model-context summarization.

## 2. The retention problem, measured without any persistence

`Service.items` is `Mutex<HashMap<String, Arc<Item>>>`. A finished item keeps its full `Finished`
projection (`result: Value`, `events: Vec<Value>`, timings). Nothing evicts it: the first work
submitted is still retrievable after thousands more.

`benches/service_retention.rs` runs the real `chip serve` release binary over TCP, with a mock model
and a PAX shim that only identifies itself (so this measures retention, not a model or PAX), submits
bounded work in waves, and reads the server's own `RssAnon` from `/proc/<pid>/status`
(`docs/product/service-retention.json`).

| Scenario (2,000 finished works after warm-up) | JSON the client sees per work | Server anon RSS growth | Per finished work |
|---|---|---|---|
| `escape`: one turn, refused, blocked (9 events) | 2.8 KB result + 1.3 KB events | +48.7 MiB | **~25 KB** |
| `read`: reads a 32 KiB file every turn until the turn limit (97 events) | 5.0 KB result + 8.8 KB events | +231 MiB | **~121 KB** |

Growth is linear with no plateau in every sample (500, 1,000, 1,500, 2,000). At ~121 KB a service
that finishes 100,000 such works holds on the order of 12 GB. In-memory cost is ~9× the JSON the
client receives, which is `serde_json::Value` overhead. Caveat: RSS includes allocator behavior; it
shows what the process holds, not a per-object accounting.

**This is the immediate problem, and it does not need a database.** Bounding finished items (a count
or age cap, with the result projection kept small) removes it. Nothing in the rest of this document
is a prerequisite.

### Follow-up: bounded retention, measured

A later change bounds it (`chip serve --max-retained-work`, default 256; see the README's
*Retention of finished work*). The same benchmark, same binary and workloads, 2,000 finished works
after warm-up, `RssAnon` of the server (`docs/product/service-retention-bounded.json`; the
1,000,000 limit stands in for the old unbounded behavior):

| Scenario | limit | retained works after 500 / 1,000 / 1,500 / 2,000 | server anon RSS after 500 → 2,000 works |
|---|---|---|---|
| `escape` | 64 | 64 / 64 / 64 / 64 | 3.8 → 4.2 MiB |
| `escape` | 256 | 256 / 256 / 256 / 256 | 8.8 → 9.6 MiB |
| `escape` | 1,000,000 | 520 / 1,020 / 1,520 / 2,020 | 14.2 → 50.4 MiB |
| `read` | 64 | 64 / 64 / 64 / 64 | 15.2 → 16.3 MiB |
| `read` | 256 | 256 / 256 / 256 / 256 | 39.5 → 42.9 MiB |
| `read` | 1,000,000 | 520 / 1,020 / 1,520 / 2,020 | 62.1 → 235 MiB |

The retained *count* plateaus exactly at the limit. RSS flattens but is not perfectly flat: it
keeps creeping by a few KiB per hundred works after the plateau (the evicted-id memory fills up to
four times the limit, and allocator fragmentation), which is why a count bound is not claimed to be
an RSS ceiling.

## 3. Candidates and acceptance notes

Stage 1 (SQLite, FeltDB, redb) was run before the scope was narrowed to "requirements first"; it is
kept as the database-level data point. The `durability`-crate journal was added after. fjall and sled
were **not** benchmarked (§8).

| | Version (pinned by `Cargo.lock`) | Maintenance (crates.io, checked 2026-10-08) | Toolchain | Notes |
|---|---|---|---|---|
| SQLite via `rusqlite` | rusqlite 0.40.2, bundled SQLite 3.53.2 | first published 2014, last release 2026-08-08, ~121 M downloads | builds on rustc 1.97 | mature C engine, bundled |
| redb | 4.3.0 | since 2018, last release 2026-10-02, ~12.9 M downloads | MSRV 1.90 | pure Rust |
| FeltDB | `rkendel1/flow_db` @ `9f2354e` | in-house | n/a | the platform's state authority; measured as session memory only |
| `durability` crate | 0.7.2, `default-features = false` | first published 2026-02-23, 23 versions in ~5 months, last release 2026-07-09, **~2.5 k downloads**, one author | MSRV 1.80 | small: crc32fast, thiserror (postcard/serde optional, unused) |

What was **verified here** (tests in this PR) versus **taken from documentation**:

| Question | SQLite | redb | FeltDB | journal on `durability` |
|---|---|---|---|---|
| Atomic multi-record update | verified: one transaction per batch | verified: one write transaction | verified: `apply_atomic_transaction` | verified: one log entry per batch; payload files ordered around it |
| Interrupted compaction | verified at 4 seams + real SIGKILL | same | same | same |
| Torn/partial final write | not separately injected; WAL recovery is documented | documented repair on open (cost visible in recovery time) | verified: partial journal line discarded | crate repairs the final segment's tail (documented); exercised by SIGKILL, not by fault injection |
| Detects a flipped byte | **no**: 0 of 5 flips refused, 3 silently changed data | **no**: 0 of 5 refused, 2 silently changed data | yes: 3 of 5 refused, 2 unaffected, 0 changed | yes: 5 of 5 refused (WAL CRC) |
| Single-process / concurrent access | one writer at a time; file locking is documented | one process; second open fails (documented) | process-global open registry | **lock file that a crash leaves behind**; recovery deletes it (`resume_after_crash`); nothing stops a second live process |
| Engine integrity checker | `PRAGMA integrity_check` ("ok" in all trials) | `check_integrity` ("ok") | none | none built in; the adapter re-checks log, checkpoints and payload parse |

The bit-flip probe (`a_single_flipped_byte_is_classified_not_assumed`) flips one byte of the store
file at 10/30/50/70/90 % after a clean close. "Changed" means the engine reopened and returned
different data without complaint. For the journal the flipped file is the first log segment; its
**payload files carry no checksum of their own** and are protected only by the observation record's
SHA-256, which compaction verifies before deleting anything and reopening does not
(`journal_payload_damage_is_caught_by_the_digest_check_not_by_reopening`). I do not describe any of
these stores as crash-safe or ACID on the strength of its documentation; §6 lists what was actually
exercised.

Finding in the `durability` crate while building the journal: a writer **resumed** after a restart is
hard-wired to flush every 64 appends through a 64 KiB buffer (there is no setter), and dropping a
writer does not flush. A tamper test that reopened a store and wrote one record lost it. The journal
adapter therefore flushes explicitly after every append. A caller who trusts the crate's default
after a restart would silently lose up to 63 acknowledged entries to a process kill.

## 4. The shared contract and what each engine is

All four implement one small trait (`Backend`: get, put, delete, ordered scan, atomic batch, a
`synced` barrier, reclaim, integrity, disk size). **Everything above it is the same code**: schema,
retention classes, pinning, preserve-before-purge compaction, orphan handling, checkpoints, reference
validation, reconstruction. The workload (`Workload`: repeated observations, 1 KiB–128 KiB payloads,
attempts, repairs, checkpoints, escalations, an unresolved failure, an open hypothesis, a pending
decision) and the **recovery packet** are the ones from the original experiment. The Stress packet
hash is byte-identical to the original FeltDB run (`321ae42f…`), which cross-checks the refactor.

The recovery packet (canonical JSON of what a recovery or escalation needs, with the SHA-256 of the
pinned diagnostic payload) was identical **across every candidate, every trial, both profiles, both
after a clean close and after SIGKILL**. No candidate got a smaller retention target or a weaker
recovery check.

| Engine | Adapter (non-comment lines) | Layout |
|---|---|---|
| FeltDB | 173 | collections of JSON keyed `coll:session:id`; revision rows, operation log, journal |
| SQLite | 210 | one table `records(coll, id, body)`, WAL |
| redb | 197 | one table keyed `(coll, id)` → JSON text |
| journal | **422** | log of record operations + checkpoint file + one file per payload, replayed into a map; the adapter **owns** the entry format, checkpoint protocol, reconciliation and payload files |

Complexity is not only lines: SQLite and redb are used through a stable API; the journal makes Chip
the owner of a crash-consistency protocol. Records are stored as JSON documents in every engine, so
these numbers are for *this adapter design* (one transaction per record, JSON), not each engine's
best case with batching or binary encoding.

## 5. Durability configuration (what a successful write guarantees)

| Engine | Ordinary write returns when | `checkpoint` / session creation barrier | Survives |
|---|---|---|---|
| FeltDB | appended to the journal and flushed to the OS (`Flushed`) | `Synced` for the checkpoint write | process death; power loss only after a barrier |
| SQLite | committed to the WAL in the OS cache (`journal_mode=WAL`, `synchronous=NORMAL`), 16 MiB page cache | `synchronous=FULL` for the checkpoint writes | process death; power loss only after a barrier |
| redb | **committed with `Durability::None`**: visible, not persisted until a later `Immediate` commit | `Immediate` | after SIGKILL: only what a barrier covered (below) |
| journal | log entry flushed to the OS; payload file renamed, not fsynced | fsync every unsynced payload file, the payload directory, then the log | process death; power loss only after a barrier |
| all `*_synced` arms | every write is fsynced (`Synced`, `FULL`, `Immediate`, fsync of log and payload files) | same | stronger: acknowledged writes survive process death and are intended to survive power loss (not tested, below) |

**These are not equivalent guarantees**, and the kill test showed it. Killing a process mid-write
(no checkpoint since creation) and reopening recovered, in the same test:

| FeltDB | SQLite | redb (default speed) | journal |
|---|---|---|---|
| 167 observations | 455 | **0** | 365 |

redb's default-speed mode is not "flush to the OS": `Durability::None` commits are lost when the
process dies, so its fair comparison is the `redb_synced` arm. Every engine recovered a consistent
session; the count is informational, the contract is consistency plus "everything up to the last
checkpoint". With every write synced the same test asserts no acknowledged write is lost, and passes
for all four.

**Process-kill tests do not establish power-loss durability.** SIGKILL leaves the operating system's
file cache intact. No power-loss or fault-injection test was run here; the `*_synced` arms configure
the strongest barrier each engine offers, which is a configuration claim, not a verified one.

## 6. Fault tests (identical for every candidate)

62 contract tests run unchanged per engine (`tests/contract.rs`, plus the original 48-test suite for
FeltDB, still passing):

- normal close and reopen reconstructs the same session and recovery packet;
- compaction keeps unresolved-failure diagnostics and open-hypothesis evidence, drops the rest, and
  leaves the packet unchanged; it is idempotent;
- compaction interrupted after preserve / during purge / before the outcome is recorded / before
  reclamation: nothing is lost, the session is never marked compacted, a rerun finishes with the same
  state as an uninterrupted run;
- repeated recovery (4×) neither duplicates nor changes anything; work after recovery adds exactly
  what it writes;
- **real SIGKILL**: during writes (consistent, no duplicates), after a committed checkpoint
  (everything up to it present), during compaction at three delays (recovers, rerun completes), and
  with every write synced (no acknowledged write lost);
- ten tampering cases refused by name (missing checkpoint reference, missing attempt, repair verified
  by a missing or failing result, test result citing a missing observation, record of another
  session, missing checkpoint record, missing pinned payload, undecodable record, missing session
  record);
- a damaged store file is refused and left unmodified; a missing session and an invalid id are
  refused; sessions are isolated and destroying one leaves another intact.

Not tested: power loss, disk-full, fsync failure, concurrent writers, a second live process, a
corrupt-but-parsable record the adapter has no checksum for (SQLite/redb above).

## 7. Results

Release build (`cargo bench`), one machine (sandbox VM, 4 logical CPUs of an Intel Xeon @ 2.10 GHz, 16 GB RAM, Linux 6.18,
glibc 2.39, rustc 1.97), the system temp directory (the filesystem type was not recorded in the raw
data), each arm and trial in a fresh child process with trials interleaved. Medians, with
min–max in the raw JSON (`docs/product/session-store-bench-{normal,stress}.json`). Memory separates
*heap in use* (application-retained), *RSS* after an explicit `malloc_trim` (allocator-retained
removed), and *disk*; the OS page cache is outside process RSS and is recorded only as an indicative
system-wide delta in the raw data. Latencies are per workload event (write) and per operation
(read), all with p50/p95/p99 in the JSON.

**Normal** (100 observations, 15 trials): sized to Chip's bounded runs. CLI/service defaults are
12 turns and 8 executions, the ceiling 50 of each, so one run at the ceiling observes about a
hundred results; payloads from the same mix (`project.read` ≤ 32 KiB, `pax.test` output up to
256 KiB in real runs; the generator caps at 128 KiB).

| | in-memory (today) | FeltDB | SQLite | redb | journal |
|---|---|---|---|---|---|
| peak RSS (MiB) | 5.0 | 12.2 | 7.5 | 7.3 | **5.2** |
| RSS after destroy + trim | 4.4 | 6.5 | 6.7 | 6.0 | 5.1 |
| ingest (ms) | 2.8 (compacting) | 24.6 | 16.0 | 13.0 | 42.6 |
| write p50 / p95 / p99 (µs) | 2.5 / 22 / 25 | 56 / 370 / 429 | 31 / 148 / 187 | 32 / 130 / 176 | 167 / 272 / 315 |
| compaction (ms) | 0.0 | 56 | 13 | 9 | 13 |
| disk before → after compaction (MiB) | – | 2.8 → 0.4 | 3.0 → 0.1 | 2.0 → 0.3 | **0.8** → 0.2 |
| clean recovery / after SIGKILL (ms) | – | 5.9 / 17.7 | 0.8 / 2.0 | 0.9 / 2.8 | 1.3 / 1.7 |

Reading Normal: a run this size costs a few MiB and a few milliseconds in every engine. The 3.3 MiB
start RSS is included in every figure, and part of each durable engine's remainder is its own code
and fixed structures rather than data. The per-write cost (30–170 µs, or 0.4–0.7 ms when fsynced) is small next
to a model call.

**Stress** (10,000 observations, 5 trials): the original workload, for comparison only.

| | in-memory baseline | in-memory compacted | FeltDB | SQLite | redb (16 MiB cache) | redb (default 1 GiB cache) | journal |
|---|---|---|---|---|---|---|---|
| peak RSS (MiB) | 82.0 | 85.5 | **681** | **31.2** | 32.8 | 171.6 | 75.8 |
| heap in use after ingest (MiB) | 78.9 | 82.4 | 575.8 | 17.2 | 16.9 | 157.9 | 35.7 |
| RSS after compaction + trim | – | 34.4 | 340.5 | **18.2** | 23.0 | 21.8 | 41.7 |
| ingest (s) | 0.21 | 0.30 | 4.8 | 2.6 | 2.0 | 1.8 | 4.6 |
| compaction (s) | – | 0.002 | 6.0 | 0.83 | 0.92 | 0.74 | 1.5 |
| write p50 / p95 / p99 (µs) | 0.3 / 0.9 / 2.4 | 2.7 / 24 / 38 | 61 / 399 / 1471 | 30 / 138 / 868 | 44 / 140 / 550 | 46 / 169 / 595 | 134 / 424 / 554 |
| disk before → after compaction (MiB) | – | – | 279 → 33.6 | **87 → 6.4** | 257 → 21.0 | 257 → 21.0 | 82 → 7.6 |
| disk / logical bytes before compaction | – | – | 3.6× | **1.12×** | 3.3× | 3.3× | 1.06× |
| clean recovery (ms) | – | – | 827 | 61 | 60 | 60 | 93 |
| recovery after SIGKILL (ms) | – | – | 3,663 | 72 | 151 | 184 | 165 |
| records lost / duplicated | – | – | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |

With every write fsynced (Stress, medians): ingest FeltDB 13.6 s, SQLite 5.0 s, redb 8.8 s, journal
10.2 s; write p50 FeltDB 730 µs, SQLite 244 µs, redb 565 µs, journal 720 µs.

Notes a reader needs:

- The in-memory columns are Task 2's arms re-run here. The baseline **does not shrink**; the
  compacted in-memory arm (82 MiB → ~9 MiB heap after compaction, ~34 MiB RSS after trim) is the
  retention improvement that needs no store.
- The journal's memory scales with the number of *records* because it replays them into a map
  (35.7 MiB heap at 10,000 observations versus SQLite's 17.2); payloads are never resident. That is
  invisible at Normal and a real cost at Stress.
- redb's file grows copy-on-write: 3.3× the logical bytes before compaction, and `compact()` leaves
  21 MiB where SQLite's `VACUUM` leaves 6.4 MiB. Its default cache is 1 GiB; the bounded 16 MiB
  setting is what the adapter uses.
- FeltDB's cost is structural (the whole state, revision rows and an operation log live in memory),
  and it is serving a different role in the platform.

## 8. Decision

1. **Is SQLite materially more efficient than FeltDB for durable session recovery?** Yes, on this
   workload. At Stress: 22× lower peak RSS (31 vs 681 MiB), 19× lower RSS after compaction and trim,
   1.9× faster ingest, 7× faster compaction, 3.2× less disk before compaction, 14× faster clean
   recovery and 51× faster recovery after SIGKILL. At Normal the gap is small in absolute terms
   (7.5 vs 12.2 MiB; 16 vs 25 ms). This says nothing about FeltDB's suitability for the platform
   state it owns.
2. **Does redb offer a meaningful advantage over SQLite after adapter complexity?** No. Memory is
   equivalent when its cache is bounded (and 5× worse at its default), ingest is 20% faster
   unsynced and 77% slower synced, disk amplification and post-compaction size are 3× and 3.3×
   worse, the adapter is not simpler (197 vs 210 lines), and its default write mode did not survive
   a process kill at all. Per the decision rule, redb is documented as measured and not preferred.
3. **Is any durable store justified for Chip's current workload?** **No.** There is no resume
   feature to recover into, and the problem that exists is `Service.items` retention (§2), which is
   fixed by bounding finished items. A durable store becomes justified only by a decision to resume
   work across restarts, together with the work-lifecycle policy that decides what is safe to retry
   (out of scope here).
4. **Retention and compaction improvements independent of any engine:**
   - bound or evict finished items in `Service.items` and keep the projection small (events beyond a
     limit, or after delivery, need not stay as `Value` trees at ~9× their JSON size);
   - treat retention classes as policy: unresolved-failure diagnostics and open-hypothesis evidence
     are pinned, everything else transient payload is purgeable — in memory this took the Stress
     heap from ~82 MiB to ~9 MiB with the identical recovery packet;
   - deduplicate repeated payloads by digest (about 30 % of the workload);
   - cap payload size at the source;
   - run compaction at checkpoints, preserve-before-purge, verified against recorded digests.

**Recommendation: no-go on adopting any store now; fix `Service.items` retention first; keep this
crate as the evaluation harness.** If durable resume is later required, the measured order of
preference is **SQLite first** (lowest memory growth, 1.1× disk, mature, the whole contract passes
with a 210-line adapter) — with an adapter-level checksum if silent corruption matters, since it
accepted 3 of 5 flipped bytes — and the **`durability` journal as the alternative** where records
stay small (it was the smallest at Normal and detected all flipped bytes in the log), accepting that
Chip would own a 422-line crash protocol on a young dependency (5 months, ~2.5 k downloads, a stale
lock file, the resume-flush trap above). FeltDB for session memory is not recommended. These are
judgements from one machine and one synthetic workload.

**fjall and sled were not run.** The condition for fjall was a measured gap in write-heavy ingest,
compaction, large payloads or storage amplification; SQLite and the journal show none at either
profile (1.06–1.12× amplification, sub-second to 1.5 s compaction, large payloads outside the hot
path), so that trigger was not met and nothing about fjall or sled is inferred. No other candidate was
added.

## 9. Guarantees that cannot be compared equivalently

- Default write durability differs (flush-to-OS for FeltDB, SQLite and the journal; `None` for
  redb), which is why the `*_synced` arms exist. Even those are different mechanisms.
- `position()` (a checkpoint's sequence and state digest) is engine-defined; the recovery packet
  excludes it.
- Torn-write handling: FeltDB and the journal tolerate a partial final record by design; SQLite and
  redb recover at transaction granularity. Only SIGKILL (not partial-write injection) was exercised.
- Integrity detection: only SQLite and redb have an engine checker; only FeltDB and the journal
  detected flipped bytes; payload files in the journal are protected only by the record digest.
- Page cache and mmap: no engine here maps files; page-cache residency was not measured per process.
- Single-writer enforcement differs (registry, file lock, lock file that needs manual clearing).
- Heap is measured via glibc `mallinfo2` for the main arena (the workload runs on the main thread);
  allocator-retained memory is separated by `malloc_trim`, but allocator behavior is not engine-neutral.

## 10. Reproduce

```sh
cargo test -p chip-session-memory                        # 48 original + 62 contract tests
cargo bench -p chip-session-memory --bench session_memory -- --profile normal --trials 15 \
  --kill-recovery --arms chip_baseline,memory_compact,felt_store_all,sqlite_store_all,redb_store_all,redb_default_cache,journal_store_all,felt_synced,sqlite_synced,redb_synced,journal_synced
cargo bench -p chip-session-memory --bench session_memory -- --profile stress --trials 5 --kill-recovery --arms …
cargo build --release -p chip-cli && cargo bench -p chip-session-memory --bench service_retention -- --works 2000
cargo test -p chip-session-memory --test contract flipped -- --nocapture   # bit-flip counts
```

Limitations: one machine, one synthetic workload (the real work-run payload mix is approximated, not
sampled from production), a mock model and a PAX shim for the retention measurement, a single
adapter design shared by all engines, process-death (not power-loss) testing, and trial counts of 15
(Normal) and 5 (Stress). Raw results, environment and method are in the JSON files above.
