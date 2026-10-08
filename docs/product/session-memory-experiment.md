# Experiment: native FeltDB as Chip session working memory

**Recommendation: NO-GO** for adopting FeltDB as session working memory to reduce memory. FeltDB's
**durability and recovery work** and are real value; the **memory benefit does not exist**: on the
same workload the FeltDB path used about **9 times** the memory of Chip's existing path while
holding the data, and about **19 times** the memory of a plain in-memory compaction after
compacting. See section 6 for the exact numbers and section 7 for what would change this.

This is an experiment, not an adoption. Nothing in Chip's product path changed. The experiment is
one leaf crate, `crates/chip-session-memory`, that nothing depends on. Remove it by deleting that
directory, its line in the workspace `members`, its row in `crates.md`, and this document.

## 1. Question

> Can Chip use its existing native Rust FeltDB implementation to maintain bounded, isolated,
> recoverable session working state, and does doing so materially improve memory behaviour without
> compromising agent recovery or escalation?

## 2. Compatibility report (Phase A)

Inspected: FeltDB (`rkendel1/flow_db` at `9f2354e`, crate `feltdb` 0.2.0) from its Rust source and
tests, not from TypeScript declarations or WASM bindings; and Chip at `6914386`.

### 2.1 FeltDB, from the Rust source

| Question | Finding |
| --- | --- |
| Crate | `feltdb` 0.2.0, workspace `crates/feltdb`; dependencies `serde`, `serde_json`, `tokio` (`sync`, `macros`, `rt`, `time`, plus `net`/`io-util`/`rt-multi-thread` off wasm32), `async-stream`, `async-trait`, `sha2`, `ed25519-dalek`, `rand`, `bincode`. No feature flags. `wasmi` is a dev-dependency only |
| Public API used | `FeltDb::open(path)`, `insert`, `insert_if_absent`, `update`, `delete`, `get`, `get_value`, `query_collection`, `list_collection_page`, `collection_cardinality`, `list_cardinalities`, `apply_atomic_transaction`, `sequence`, `state_digest`, `set_durability_mode`, `StateStore::{with_feltdb, collect_unreachable, set_retention_policy, history_of}`, `add_sync_peer`, `acknowledge_peer_versions`, `compact_operation_log`. Errors are `FlowError` |
| In-memory backend | **None.** `FeltDb::open` takes a path and always creates a journal file and a `<path>.lock` ownership file. `MemoryStorage` is re-exported but `FeltDb` never uses it (it is an append-log helper with no get/query/delete). `StateStore::new_volatile()` is documented "for testing state semantics in isolation... production must use `with_feltdb`". Neither is a session store |
| Smallest supported mode | The file journal in a temporary directory. Filesystem effects, asserted in `native_proof.rs`: exactly two files, `x.felt` and `x.felt.lock` |
| Independent instances in one process | Yes for different paths. **For the same path, `open` returns the same instance** through a process-global registry (`OPEN_DATABASES`, weak references, keyed by canonical path): two handles share all state (asserted in `isolation.rs`). The adapter avoids it by giving each session a fresh directory and refusing an existing one |
| Process exclusivity | Ownership is an OS file lock held while any handle lives; a second *process* is refused with "already owned" (FeltDB's own `local_process_ownership.rs`). The lock dies with the process (shown here with a real SIGKILL) |
| Threads / runtime | Opening starts no thread (asserted). It links `tokio` as a library; Chip already depends on it. No runtime is started, no service, no Node, no WASM host |
| Transactions / concurrency | One mutex serializes all operations on a store. `apply_atomic_transaction` is all-or-none with idempotent ids. Multi-step procedures built on top are not isolated; this experiment does not claim concurrent writers to one session |
| Persistence | Append-only JSON-lines journal, flushed to the OS before each call returns (`Flushed`, the default); `Synced` adds an `fsync` per write. Reopen replays the journal. An unterminated final record is discarded and reported; corruption in the middle **refuses the open and modifies nothing** (asserted) |
| Deletion | `delete` appends a tombstone and removes the row from memory. It **does not** release the payload: every write also mints a **revision row** (under `state`) holding a copy of the payload, and an **operation** in the in-memory change log holding another. After deleting 50 records the 50 revision rows remain and the deleted payload is still readable through `StateStore::history_of` (asserted) |
| Reclamation mechanisms | (a) `StateStore::collect_unreachable` removes revision rows; with no refs it removes *all* revision history, live records' included. (b) `compact_operation_log(peers)` prunes only operations every listed peer has acknowledged and **returns immediately with no peers**; with one acknowledging peer it prunes and rewrites the journal as a snapshot. (c) `set_retention_policy(keep_last(n))` bounds revisions per resource if set before the writes, at the price of a retention row per resource. No supported call returns memory to the operating system |
| Shutdown | There is no `close()`. Dropping the last handle releases the store, including its ownership lock (asserted). After the drop the process heap in use falls to the baseline; the allocator still holds what it reserved |
| Existing tests that establish this | `crash_durability_contract`, `durable_corruption_contract`, `local_process_ownership`, `compaction_stall_contract`, `pr34_query_collection`, `pr35_equality_index`, `durable_backup_contract` in `crates/feltdb/tests/`; and the assertions in `crates/chip-session-memory/tests/feltdb_facts.rs`, which re-establish the facts this design depends on |

### 2.2 Chip

| Question | Finding |
| --- | --- |
| Session | **There is none.** The unit is one bounded work run (`WorkId`, `Agent::run_work_with_context_policy`, `crates/chip-core/src/work.rs`). There is no session identity across runs, and no persisted form of a run |
| What a run retains | A `Run` holds `events`, `observations` (the **full** output of every capability), `origins`, `decisions`, `ruled_out`, `escalations`, for the run's duration, then returns them in an owned `WorkReport` |
| Bounds | `WorkLimits { max_turns, max_executions }`, defaults 8 and 4 (12 and 8 on the CLI and service, ceiling 50). So the product path holds at most a few dozen observations per run. **The benchmark's 10,000 observations are about 200 times the product's envelope** |
| Context | Each model call rebuilds its context from *all* observations in full, minus identical repeats of reusable capabilities (`DeduplicatedEscalationContext`, by `omissions()`); a `context_budget_bytes` refuses a call that would exceed it. Nothing summarizes, truncates or compacts |
| Escalation | A `WorkOutcome::Escalated` ends the run. There is no resume |
| Retention elsewhere | `Service.items` keeps every finished work's JSON result and event list in a `HashMap` for the life of the process; nothing removes them (`crates/chip-cli/src/service.rs`). `docs/product/performance.md` lists long-running memory as unmeasured |
| Serialization | `chip.work-decision.v1` for model decisions and `render_json` for reports. No checkpoint format |
| Measurement facilities | `ContextReport`, `WorkLatency`, `benches/baseline.rs` (RSS growth over 200 ten-step works, ~0) |
| Smallest integration point | A new object owned by the run that records the *recovery-class* facts of a run. Not built: the product has no session to attach it to (see section 7) |

### 2.3 Answers to the Phase A exit questions

1. **Can Chip call the native Rust API directly?** Yes. It compiles inside the existing workspace as
   a pinned git dependency, with no other runtime. (`native_proof.rs`)
2. **Is isolated in-memory operation supported?** No.
3. **What storage mode is there?** A file journal plus an ownership lock, in a directory the
   caller owns.
4. **Can multiple stores coexist safely?** Different paths, yes. The same path, no: it is one
   shared instance.
5. **Which guarantees are supported?** Durable acknowledged writes surviving process death;
   all-or-none batches; refusal of a corrupt journal; release on drop. **Not** supported without
   repurposing the replication API: pruning the operation log; **not** offered at all: returning
   memory to the operating system, and an in-memory mode.
6. **Smallest integration point?** See above: there is no session in Chip to integrate with.

## 3. Native integration proof (Phase B)

`crates/chip-session-memory/tests/native_proof.rs`, two tests against the real crate: open an
isolated store, write, read back, update, query, delete (logically absent), provoke two error paths
(`PreconditionFailed`, `Serde`) and see them as typed errors, drop, reopen the same path and see the
journal, open a different path and see no leaked state. Passed first time.

Dependency boundary, checked with `cargo tree`: `feltdb` has exactly one dependent
(`chip-session-memory`), and no Chip, FX, PAX, Compute, WASM, HTTP or database crate is in the
adapter's normal dependency graph. The only `chip-core` edge is a **dev-dependency** used by the
benchmark's baseline arm. `scripts/audit-dependencies.sh` (which already forbids core crates from
depending on `feltdb`) still passes.

## 4. Adapter, schema, retention classes (Phases C and D)

`crates/chip-session-memory/src/`: `store.rs` (lifecycle and records), `schema.rs`,
`compaction.rs`, `recovery.rs`, `workload.rs`, `error.rs`.

* **Isolation is structural**: a session is its own journal in its own directory. Every key is
  `{collection}:{session}:{id}` and every record carries its session; a record naming another
  session is an error, not a result.
* **Lifecycle**: `create` (refuses an existing session; removes what it created if it fails),
  `recover`, `close` (idempotent), `destroy`. Disabled by default: only a test or the benchmark
  constructs one. Single-writer per session; independent sessions may use separate threads.
* **Typed errors** per stage: `Init`, `Persist`, `Query`, `Compaction{phase}`, `Checkpoint`,
  `Recovery`, `ForeignRecord`, `MissingReference`, `Invalid`, `Closed`, `Unsupported`.
* **Schema** (ten logical categories in twelve small collections): session, task, plan, attempt,
  observation, payload, test result, repair, hypothesis, escalation, checkpoint, compaction record.
  Every record has a schema version; references are validated when written and again on recovery.
* **Nothing is evidence.** An observation's provenance is an opaque string the caller supplied and
  is kept verbatim; nothing mints or derives an execution identity or a receipt. A repair is
  `verified_by` a test result only if that result exists **and passed**; a text claim cannot verify.

| Retention class | Contents | Fate |
| --- | --- | --- |
| Working | the plan, open tasks, open hypotheses | kept |
| Recovery | attempts, repairs, test results, escalations, checkpoints, every observation's small record (summary, digest, length, excerpt, provenance) | kept; recovery-class records cannot be deleted by a caller |
| Transient | the full payload of an observation | purged by compaction, **unless pinned**: the diagnostic of a failure that is still its command's current reading, and the evidence of an open hypothesis |

**Compaction** is deterministic, idempotent and resumable: select; preserve (check each purge
candidate's summary, digest, length and that the stored payload matches its digest, then persist a
compaction record naming exactly what will be purged); validate (read it back and reconstruct the
session without the payloads); purge in atomic batches; verify; record; reclaim; and only then
mark the session compacted. Nothing is deleted before the record naming it is written and read
back, and an interrupted compaction is never counted as done.

## 5. Tests (Phases E and F, section 9 of the brief): 48 + the benchmark

`cargo test -p chip-session-memory` (7 test files). Highlights:

* **Recovery from persisted data**: a session with failed attempts, a verified repair, test
  failures, an unresolved question and an escalation is checkpointed, compacted, **destroyed**
  (no handle survives), and reconstructed from the journal: objective, status, plan, attempts,
  failed approaches, verified repairs, unresolved failures and their diagnostic references, open
  hypotheses, escalation context, the pending decision, the checkpoint and all its references.
  Purged payloads are absent; what survives is consistent.
* **A real process kill**: a child process writes continuously and is `SIGKILL`ed; the session
  recovers to what it had acknowledged and can then be compacted and checkpointed again. This is
  process-restart recovery of acknowledged writes, and it is the only recovery claim made about a
  process boundary.
* **Recovery refusals**, each a named error: a missing session, a corrupted journal (left
  byte-for-byte untouched), a newer schema, a checkpoint with a dangling reference, a repair
  "verified" by a missing or failing test, a foreign record, a lost pinned payload.
* **Failure injection** at every compaction seam (after preservation, mid-purge, before recording,
  before reclamation), a tampered payload that stops compaction before anything is deleted, and an
  orphan payload from an interrupted write. Two real bugs in the adapter were found this way
  (resume validation; an interruption after the last delete) and fixed.
* **Isolation**: identical ids in two sessions do not collide; compaction in one leaves the
  other's data, footprint and journal bytes unchanged; destroying one leaves the other usable;
  four sessions on four threads. And the hazard FeltDB itself has (same path shares an instance).
* **Unsupported capabilities** are reported by `capabilities()` and asserted, not skipped.

Not tested, and not claimed: concurrent writers to one session; behaviour on a power failure
(only process death); other filesystems or platforms.

## 6. Benchmark results (Phase G)

`cargo bench -p chip-session-memory --bench session_memory`. 10,000 observations (payloads 60% 1
KiB, 30% 8 KiB, 9% 32 KiB, 1% 128 KiB; about 30% repeating an earlier payload exactly; 72 MiB of
payload in all), multiple failed attempts and repairs, test failures and passes, superseded
observations, 11 checkpoints, 5 escalations, an unresolved failure and a pending question, and a
final recovery. 5 trials, each arm and trial in a fresh process, interleaved. Linux 6.18, Intel
Xeon 2.1 GHz, 4 CPUs, 16 GiB, glibc 2.39, release build, journal in the default temp directory.
Raw data: [`session-memory-bench.json`](session-memory-bench.json) (units, configuration,
environment, method, every trial). Medians below; the standard deviation across trials is under 1%
for allocator heap points, and up to about 7% for FeltDB's RSS (716.8 plus or minus 53.5 after ingest), which depends on how the allocator happened to grow.

Four arms:

* `chip_baseline`: **Chip's existing path**: the real `Observation` and `ObservationOrigin`
  structures held for the whole run, and the real `omissions()` deduplication used to build the
  escalation context. It has no compaction and no persistence.
* `memory_compact`: a plain in-memory session that keeps everything, then drops transient
  payloads. Not Chip's path: a reference for what compaction alone achieves **without FeltDB**.
* `felt_store_all`: the FeltDB adapter keeping every payload until compaction.
* `felt_summaries_only`: the FeltDB adapter keeping only test diagnostics and dropping the payloads
  of reads, lists and searches at ingest.

| MiB unless stated | chip_baseline | memory_compact | felt_store_all | felt_summaries_only |
| --- | ---: | ---: | ---: | ---: |
| RSS at start | 2.8 | 2.7 | 2.8 | 2.7 |
| **RSS after ingest** | **81.2** | 84.8 | **716.8** | 225.8 |
| heap in use after ingest | 78.9 | 82.4 | 576.7 | 212.7 |
| peak RSS after compaction | n/a | 85.1 | 752.0 | 269.6 |
| **heap in use after compaction** | n/a | **9.2** | **176.9** | 122.3 |
| RSS after compaction | n/a | 78.7 | 752.0 | 251.6 |
| RSS after compaction + `malloc_trim` | n/a | 33.5 | 342.7 | 205.4 |
| RSS after instance destroyed | 73.3 | 31.4 | 285.4 | 139.3 |
| RSS after destroy + `malloc_trim` | 7.0 | 6.5 | 19.1 | 20.2 |
| heap in use after recovery from disk | n/a | n/a | 99.1 | 76.6 |
| logical payload before / after | 72.2 / 72.2 | 72.2 / 0.0 | 72.2 / 0.0 | 3.0 / 0.0 |
| records before / after | 10,001 / 10,001 | 10,001 / 10,001 | 20,923 / 10,924 | 11,323 / 10,924 |
| journal on disk before / after | n/a | n/a | 280.0 / 33.6 | 51.6 / 22.6 |
| ingest, ms | 214 | 284 | 4,115 | 1,941 |
| compaction, ms | n/a | 2.5 | 6,138 | 2,554 |
| checkpoint, ms | n/a | n/a | 264 | 110 |
| recovery from disk, ms | n/a | n/a | 910 | 583 |
| write p50 / p99, microseconds | 0.3 / 2.4 | 2.7 / 33.8 | 58.9 / 1,343 | 32.0 / 1,215 |
| read p50 / p99, microseconds | 0.2 / 0.6 | 0.2 / 0.6 | 6.5 / 20.4 | 4.0 / 9.4 |
| full scan of observation records, ms | 0.1 | 0.0 | 56.6 | 61.0 |

Also measured: the recovery packet (the canonical facts a recovery or escalation needs, 11.4 KB)
is **identical across all four arms in all 20 trials**, including the SHA-256 of the pinned
diagnostic payload, so no arm lost anything recovery needs. Chip's current escalation context
for this session would be **69.9 MiB** (every observation not omitted as a repeat, in full); the
recovery packet is 11.4 KB. The process has one thread in every arm, at start and at the end.
Serializing 10,000 observation records takes 5.7 ms and deserializing them 9.7 ms: not a cost.

The five quantities the brief asked to keep apart, for `felt_store_all`:

| | MiB |
| --- | ---: |
| logical payload | 72.2 |
| records FeltDB itself tracks before compaction | 20,923 records plus 24,346 revision rows |
| heap in use after ingest (allocator) | 576.7 |
| process RSS after ingest | 716.8 |
| returned to the OS by an explicit `malloc_trim` after compaction | 752.0 to 342.7 (409 of it); after destroy a further 266, leaving 19.1 |

### What the numbers say

1. **Holding data costs about nine times more through FeltDB.** 72 MiB of payload is 81 MiB of RSS
   in Chip's current path and 717 MiB through `felt_store_all`: each write is held as the row, as
   a revision row and as an operation in the change log, plus the journal. Journal bytes are 3.9
   times the payload.
2. **Deleting does not reclaim; compaction reclaims logically and only partly physically.** Delete
   alone changed nothing measurable (and the journal only grows). Garbage collection of revisions
   plus acknowledged log compaction rewrote the journal from 280 to 34 MiB and collected 24,351
   revisions and pruned 68,702 operations. But **live heap after compaction is still 177 MiB**
   for 1.2 MiB of logical records, against 9 MiB for the same compaction done in plain memory:
   FeltDB keeps its derived and bookkeeping state for the life of the store. Peak RSS *rose*
   during compaction (752 against 717 MiB).
3. **Memory goes back to the allocator when the store is dropped, and to the OS only with an
   explicit trim.** After `close()` the heap in use is 0.5 MiB; RSS stays at 285 MiB until
   `malloc_trim`, then 19 MiB. That is a property of glibc, not of FeltDB, and it applies to the
   baseline as well (73 to 7 MiB).
4. **The structured state alone is not cheap.** `felt_summaries_only`, which stores almost no
   payload (3 MiB), still takes 226 MiB of RSS and 122 MiB of live heap after compaction; a
   session recovered from disk holds 77 to 99 MiB of heap for about 11,000 small records.
5. **Latency is worse by two orders of magnitude** (write p50 59 µs against 0.3 µs; compaction
   6.1 s for a 10,000-observation session) but is small against model latency, and not the
   deciding factor.
6. **What FeltDB adds that the baseline lacks is persistence.** Chip's path cannot recover at
   all (a process restart loses the run); the FeltDB arms recover the identical packet from disk
   in 0.6 to 0.9 s.

## 7. Answers to the brief's questions

* **Did native Rust integration work without an additional runtime?** Yes. A pinned git
  dependency in the existing build; no thread, service, Node or WASM host.
* **Is a supported in-memory backend available?** No. `MemoryStorage` and `StateStore::new_volatile`
  exist but are not stores `FeltDb` uses or that a session could use.
* **Can multiple independent instances coexist safely?** Yes, on different paths. On one path they
  are one instance, which the adapter's directory-per-session layout prevents.
* **What happens to memory when records are deleted?** Nothing, until revisions are collected and
  the operation log is pruned and the journal rewritten; and then it goes to the allocator, not the
  OS, until the store is dropped and the allocator trimmed.
* **Is physical reclamation supported and measurable?** Partly. Revision collection and journal
  rewrite are supported and measured (journal 280 to 34 MiB). Pruning the operation log without
  replication peers is **not** a supported use: it works by acknowledging operations on behalf of
  one nominal local peer, which repurposes the replication API. Returning memory to the OS is not a
  FeltDB feature.
* **Does compaction preserve sufficient task and escalation context?** Yes, shown by identical
  recovery packets in every trial and by the recovery tests, including after interruption.
* **Does recovery work after the instance is destroyed?** Yes, from the journal, and after a real
  SIGKILL for acknowledged writes. Not shown: power loss.
* **What memory and latency changes were measured?** Section 6. Memory is far worse; latency is
  worse and small in absolute terms; persistence is new.
* **Trade-offs?** Operability and recoverability against a roughly nine-fold memory cost and a
  dependency on an 84,000-line crate whose reclamation story for this use depends on repurposing
  its replication API.
* **Is native FeltDB session memory simpler and measurably better than the existing Chip path?**
  **No.** It is more machinery, and measurably worse on the one thing the experiment was meant to
  improve. Better only on durability, which Chip's path does not have.

## 8. Go / no-go

**NO-GO** on the stated purpose, for three reasons that are each sufficient:

1. *Compaction hides records without addressing the actual problem*: after compaction FeltDB still
   holds 19 times the live heap of an in-memory compaction, and its RSS is higher than the
   baseline's before any compaction at all.
2. *The additional storage overhead outweighs the benefit* (section 6, points 1 to 4).
3. *The reclamation Chip would need is outside FeltDB's supported contract* (pruning the operation
   log with no peers).

It is **not** a finding that FeltDB is unsuitable as durable state. Where a durable, recoverable
session across restarts is itself the requirement, the adapter shows it can be done with the real
API, with identical recovery. The honest framing is that durability, not memory, is what FeltDB
would buy, and Chip has no requirement for it today: it has no session, and its runs are bounded
to a few dozen observations.

### What would change the answer (each is a separate decision)

* A requirement for sessions that outlive a process or a run (resume after escalation is the
  obvious one; the coding-agent evaluation found Chip cannot resume). Then the cost is the price
  of durability and should be measured against that, with a retention policy set per resource
  before writing, and payloads kept out of the store (references only), which the
  `felt_summaries_only` arm approximates.
* An upstream FeltDB mode without revision rows and without an unpruned operation log for
  single-node use. That is a change to FeltDB, not to Chip.
* If the actual problem is accumulated transient state in a run or in the service, a plain
  in-memory fix is **simpler and measurably better** than anything here: `memory_compact` frees
  the same 72 MiB in 2.5 ms and ends at 9 MiB. The service's unbounded `items` map is the more
  concrete instance of the problem and needs no storage engine.

Keep three concerns separate: *working memory* (ordinary Rust structures with explicit retention
and compaction), *durable session recovery* (evaluate a storage engine only if cross-run persistence
or resume after escalation becomes a real requirement), and *platform state* (FeltDB keeps its
intended durable-state role). If durable session storage does become necessary, benchmark an
embedded SQL store such as SQLite (for example through `rusqlite`) against FeltDB with the same
recovery contract and workload. **SQLite was not tested here and nothing in this experiment shows
it to be better.**

Any WASM-hosting or alternative-store investigation requires its own decision and PR.

## 9. Reproduction

```sh
# FeltDB is a git dependency pinned to a commit; cargo fetches it (needs network once).
cargo test  -p chip-session-memory                      # 48 tests incl. a real process kill
cargo bench -p chip-session-memory --bench session_memory                 # 10,000 obs, 5 trials
cargo bench -p chip-session-memory --bench session_memory -- --quick      # 2,000 obs, 3 trials
#   options: --observations N --trials N --seed N --out PATH
#   output:  target/session-memory-bench.json (or SESSION_MEMORY_BENCH_JSON)
sh scripts/audit-dependencies.sh                         # dependency direction still holds
cargo test -p chip-cli --test inventory                  # the crate inventory
```

* Crates: `feltdb` 0.2.0 at `rkendel1/flow_db` commit
  `9f2354e89743bf1bdc8f1fc825d8259fd80920fe`; `serde` 1, `serde_json` 1, `sha2` 0.10; dev:
  `tempfile` 3, `chip-core` (path). Rust 1.97, glibc 2.39 (the benchmark uses `mallinfo2` and
  `malloc_trim`, glibc-specific, and reads `/proc`; it is Linux only).
* Memory is read from `/proc/self/status` (`VmRSS`, `VmHWM`) and glibc `mallinfo2` for the main
  arena; the workload runs on the main thread. `VmHWM` only rises, so "peak during compaction" is
  the high-water mark after it and whether that exceeded the mark before it.
* The benchmark's baseline arm uses Chip's real `Observation`, `ObservationOrigin` and
  `omissions()`. It does not run the work loop (the loop caps a run at 50 turns), so it measures
  Chip's retention *structures* at a scale the loop would never reach. That is a stated limit, not
  a hidden one: the workload is 200 times what Chip's own limits allow.
* Limits of this evidence: one machine; one filesystem; process death, not power loss; FeltDB's
  default durability; revision retention not enabled in the adapter (a mitigation worth trying
  only if the answer to section 8 changes); no live model; a synthetic but deterministic workload.
