# Session store format (`event_log_v1`)

Persistence keeps its existing owner: the directory format the Zig `fx` writes under
`~/.fx/sessions/<session id>/`. Rust Chip reads and (in a later slice) writes this same format. There is
no second session database. The facts below were taken from sessions written by the real Zig binary
(`crates/chip-session/tests/fixtures/zig`, produced by `docs/rebuild/tools/capture_zig_sessions.py` in
rkendel1/fx) and from `src/core/session/session_log.zig` and `session_event.zig` there.

## Files that define a session

| File | Role | Truth or projection |
| --- | --- | --- |
| `events.jsonl` | the append-only event log, one frame per line | truth |
| `commit.<log_generation>.json` | the committed watermark of one generation | truth (what is durable) |
| `authority.json` | which implementation created the session | truth |
| `session.json` | manifest: counts, preferences, positions, checkpoint digest | projection, rebuilt from the log |
| `checkpoint.json` | reduced state at an earlier sequence, with its own digest in the manifest | projection |
| `display.json`, `usage-v2.json` | list title, usage snapshot | projection |
| `session.lock`, `commit.lock` | single-writer locks | coordination |
| `commit.pending.json`, `authority.pending.json` | intents of an interrupted publication | recovery |

Directories are mode 0700 and files 0600 when written. Readers refuse symlinks, directories in place of
files, and files with more than one hard link.

## Session id

1 to 255 bytes of `[A-Za-z0-9._-]`, not `.` or `..`. Zig generates
`<ms>-<ns>-<16 hex>`; any valid id is read.

## Event frame

One line, at most 8 MiB including the newline, exactly these keys in this order (a reader requires the set,
no extras, no duplicates):

```
{"schema_version":1,"log_generation":"<32 hex>","seq":N,"event_id":"<32 hex>","timestamp_ms":N,"kind":"<kind>","payload":{...}}
```

Kinds: `session_started`, `preferences_changed`, `workspace_rebound`, `history_turn_committed`,
`usage_checkpointed`, `recovery_checkpoint_set`, `recovery_checkpoint_cleared`,
`state_replacement_started`, `state_replacement_chunk`, `state_replacement_committed`. Identifiers are 32
lowercase hex characters. Sequence numbers are contiguous from the generation base (1 for a new session).
The payload is opaque to the store; Rust re-encodes frames to exactly the bytes Zig wrote (tested).

## Committed watermark

`commit.<generation>.json`, one line, exact keys `schema_version` (1), `session_id`, `log_generation`,
`through_seq`, `through_event_id`, `through_event_log_bytes`. The live generation is the first frame's
`log_generation`. A log is valid to the watermark when every line up to `through_event_log_bytes` is a frame
of that generation, sequence numbers are contiguous, and the last frame is exactly the one named. Bytes past
the watermark are an unfinished append: reported as an uncommitted tail, never applied.

## Authority

`authority.json`: exact keys `schema_version` (1), `session_id` (equals the directory name), `authority_id`
(32 hex), `storage_format` (`event_log_v1`), `source` (`native_create` or `legacy_migration`).

## Commit protocol (writing)

Taken from `publishFrames` and `createNativeSession` in `session_log.zig`; `chip-session` follows it step for step.

Append: (1) write the frames at the committed end (dropping any longer, unpublished tail first) and sync the log; (2) take `commit.lock`; check the watermark is still the prior position and no intent exists; (3) durably write `commit.pending.json` (`kind: watermark_advance`, `prior`, `proposed`, an operation id); (4) durably replace `commit.<generation>.json`; (5) read the watermark and appended bytes back; (6) remove the intent and sync the directory. "Durably" means temp file `.<name>.tmp.<hex>` created 0600, written, synced, renamed over the target, directory synced.

A failure before the intent rolls the tail back (nothing published). A failure after it republishes the prior position. Never retried.

Create: `mkdir` 0700; `events.jsonl` (exclusive, 0600) with the `session_started` frame at seq 1; sync; durable watermark; durable `authority.pending.json` (`session_create`); durable `authority.json`; remove the intent.

Open for writing: require mode 0700 / 0600 and single hard links; take `session.lock` (flock, 2 s deadline, `Locked` on timeout); under `commit.lock` resolve an interrupted creation, then an interrupted publication:

* watermark equals the proposed position and the log is valid there: roll forward;
* watermark equals the proposed position but the log is not valid there: require the prior to be valid, truncate to it, republish it;
* watermark equals the prior: require the prior to be valid and drop the unpublished tail;
* anything else: `CommitIndeterminate`, refused.

Then remove the intent. An unreadable or foreign intent is refused and left in place.

Only the truth files are written. The projections are rebuilt by whoever resumes the session; the Zig binary does this on resume (verified, below).

## What `chip-session` does today

* Reads one session directory read-only and proves the committed range. A missing, stale or invalid projection is reported (`ManifestStatus`, `CheckpointStatus`), not repaired.
* `SessionWriter::{create, open, append}` with crash-injection points (`Boundary`) used by the tests to model a process dying at each step.

## Evidence

* `crates/chip-session/tests/zig_fixtures.rs` (16): Zig-written sessions read and verify; frames re-encode to exactly Zig's bytes; corruption is refused.
* `crates/chip-session/tests/writer.rs` (15): the writer's watermark and authority bytes equal Zig's for Zig-written sessions; a crash at every append boundary resolves to the prior or proposed position and the session keeps working; torn tail over a published watermark rolls back; stale tail replaced; malformed or foreign intent refused; a crash during creation never yields a half session; second writer `Locked`; non-private modes refused.
* Cross-implementation, with the real Zig binary: `docs/rebuild/tools/verify_rust_session_resume.py` in rkendel1/fx. Zig creates a session; Rust appends; Zig resumes and runs another turn; Rust reads a contiguous log with both writers' events. Rust creates a session; Zig resumes it. A negative control (Rust appends an event Zig rejects) shows the check can fail.
  Run locally; it needs the Zig binary and is not yet a CI job.

## Not done (blockers for retiring the Zig store)

* Rebuilding the projections (`session.json`, `checkpoint.json`, `display.json`, `usage-v2.json`), `latest/` and the session index, child (subagent) sessions, legacy migration, log compaction and state replacement.
* Payload meaning: turns, usage, recovery checkpoints. Callers pass payload JSON; the store does not interpret it.
* The cross-implementation check is not in CI; macOS is untested (fsync semantics differ).
* A concurrent Zig process and a Rust writer share the same advisory lock files (`flock`), which is why the second writer is refused, but that is covered only by the same-process test.
