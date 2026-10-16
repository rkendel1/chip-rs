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

## What `chip-session` does today

Reads one session directory read-only and proves the committed range. A missing, stale or invalid
projection is reported (`ManifestStatus`, `CheckpointStatus`), not repaired; the manifest's
`checkpoint_sha256` is verified against `checkpoint.json`.

## Not done yet (blockers for retiring the Zig store)

* Writing: create a session, append with the Zig commit protocol (append, sync, publication intent, watermark
  rename, directory sync), locking, recovery of an interrupted publication, rollback of a failed tail.
* Proof that the Zig binary reads a Rust-written session (resume).
* Payload meaning: turns, usage, recovery checkpoints, state replacement and compaction.
* Projection rebuild (`session.json`, `checkpoint.json`, `display.json`), `latest/` and the session index,
  child (subagent) sessions, legacy migration.
