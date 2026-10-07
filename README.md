# chip-rs

A small Rust workspace that proves a clean model boundary between the Chip agent runtime and provider execution.

## Workspace

- `crates/fx-core`: provider-neutral model boundary
- `crates/chip-core`: agent runtime
- `crates/chip-cli`: minimal command-line demo

## Usage

```bash
cargo check --workspace
cargo test --workspace
cargo run -p chip-cli -- --test
```

The CLI uses a deterministic in-memory provider and completes a single turn without external credentials or services.

## Software verification agent (`chip-cli verify`)

```sh
cd ~/src/project
chip-cli verify            # human-readable
chip-cli verify --json     # machine-readable (extends the existing work measurement JSON)
```

Chip is the agent. PAX is an **external** project capability. Cargo, npm and the like are the native
project tooling PAX drives. They are independently versioned; Chip does not bundle, install,
download or upgrade PAX, and there is no combined Chip/PAX version.

```
model ──judgment──▶ Chip ──pax.test──▶ pax --dir <cwd> --json test ──▶ native tooling
                     ▲                                  │
                     └── validated pax.execution-result.v1 (status decides the goal)
```

* **Goal** (fixed): "Verify that the project's tests pass." It is satisfied only when PAX's own
  `status` is `passed`: never a native exit code, never output text, never a model's claim.
* **Capability** (exactly one): `pax.test`, with no inputs. The model cannot supply a project path,
  executable, arguments, status, observation or receipt. The project is the current directory.
* **Terminal states**: `Completed` (goal satisfied), `Blocked` (executed or evaluated, goal not
  satisfied: `failed`, `not_run`, `unsupported`, `ambiguous`, `error`, or a refused completion claim),
  `Failed` (malformed decision, unknown capability, malformed PAX result, runtime failure).
* **Exit status** (Chip's, never a native exit code): `0` verified; `1` not verified; `2` usage;
  `3` infrastructure unavailable (no provider or no usable PAX; nothing ran); `4` runtime failure
  or a safety-invariant violation.
* **Receipts**: PAX issues none and Chip invents none. The evidence is a verified observation of
  PAX's result, which is not a cryptographic execution receipt.

### Requirements

* A model provider behind FX, configured as for the rest of `chip-cli`: `CHIP_PROVIDER`,
  `CHIP_MODEL`, and `CHIP_ENDPOINT` / `CHIP_API_KEY` where the provider needs them.
* PAX **0.3.0 or later** (`pax.execution-result.v1`), found as `$PAX_BIN` if set, otherwise as the
  first `pax` on `PATH`. The candidate must identify itself via `pax --version`; the POSIX `pax`
  archive utility is refused. Chip never searches for another candidate. Use `PAX_BIN=/path/to/pax`
  when `PATH` discovery is not enough.
* The native tooling PAX needs for your project (for example `cargo`).
* Not required: Compute, Attn or any other part of the wider stack.

`chip-cli --version` prints Chip's own version only.

## Software work agent (`chip-cli work`)

```sh
cd ~/src/project
chip-cli work "Fix the failing tests in this project"                       # model from the environment
chip-cli work --provider ollama --model qwen3-coder "Fix the failing tests"  # fully local
chip-cli work --provider anthropic --model claude-haiku-4-5-20251001 --json "<goal>"
```

**Choose the model. Chip controls the work.** Chip is a bounded autonomous work runtime; file
navigation, editing and test running are *capabilities* it offers the model, not the shape of the
agent. The model supplies judgment; Chip validates every request, performs every operation itself,
observes the result and decides from those observations whether the goal is met. Every model gets
the same boundary: nothing about the provider or model identity grants authority.

### Selecting the model

Precedence, the same every time: command line (`--provider`, `--model`, `--endpoint`), then the
environment (`CHIP_PROVIDER`, `CHIP_MODEL`, `CHIP_ENDPOINT`, `CHIP_API_KEY`), then the provider's
default endpoint, and then nothing: **a model is always required and never inferred from the
provider.** Ollama's default endpoint is `http://127.0.0.1:11434`, so a local run needs no external
request. There is no model discovery, no fallback and no retry: if the selected provider or model
cannot answer, the run says so (`error: the selected model did not answer (...). No other provider or
model was tried.`) and stops. `--json` reports `provider`, `model` and the endpoint's identity
(scheme, host, port: never a path, userinfo or key).

### Capabilities

| capability | inputs | what Chip does |
|---|---|---|
| `project.list` | `path` (optional; `.` or absent = the root) | lists the entries directly in a project directory (at most 200) as `dir`/`file` rows with project-relative paths |
| `project.search` | `query`, `path` (optional) | literal, case-sensitive substring search: `path:line: text` rows; at most 500 files examined (each at most 256 KiB), 50 matches, 16 KiB of output; non-UTF-8 and oversized files are skipped and counted |
| `project.read` | `path` | reads a UTF-8 file (at most 32 KiB) and records its real content |
| `project.write` | `path`, `content` | atomically creates or replaces a UTF-8 file (at most 32 KiB), reads it back, records what the filesystem holds |
| `pax.test` | none | runs `pax --dir <project> --json test` and records PAX's `pax.execution-result.v1` |

* **The model owns only a project-relative path, a literal query, and file content.** It cannot
  supply a root, an absolute path, `..`, a symlinked path, a command, an executable, arguments, a
  working directory, an environment, a status, an observation, evidence, an execution id or a
  receipt. A request that tries is rejected before anything executes: no execution, no observation,
  no evidence, no filesystem access. `.git`, `.env*` and symlinks are never listed, searched, read or
  written; hidden entries and `target`/`node_modules` are not searched. Directories are not created.
* **What an observation establishes** comes from its capability's contract: a read, that Chip read
  this file at that point; a search, that Chip searched that bounded scope and found these matches
  (not that they are all the relevant ones); a test result, PAX's own `status`. None of them says the
  code is correct. Only a PAX `passed` after the last change satisfies the goal.
* **There is no shell, command or process capability.** Project tooling is reached only through PAX.
  Inherent limit: code the model writes is run by the project's own test tooling, so `project.write`
  plus `pax.test` is code execution *through the project's tooling*. Chip bounds what the model may do
  to files; it does not sandbox the project.
* **Goal.** Complete only when PAX established `passed` *after the last change* that altered a file. A
  pass that predates a later change does not count, a write alone is not completion, and a model's
  claim of completion is refused until reality supports it. The runtime completes the work itself as
  soon as the goal holds.
* **Revisiting a capability.** A note that one invocation failed or fell short names that invocation
  (`project.read (path="src/a.rs")`); another input to the same capability is a different invocation
  and is never ruled out by it. Every read, list, search, write and test run is performed again when
  asked, never answered from remembered evidence.
* **Budgets.** The existing `WorkLimits` (turns, executions) bound everything; every execution spends
  the same budget. A reply may use up to 2048 tokens (a reply that writes a file needs room; this is a
  budget, not an authority). No retries, no hidden repair.
* **Audit.** The existing safety auditor, extended with `path_escape`, `out_of_root_write`,
  `host_path_leak` (no project observation contains the host path), `navigation_mismatch` (every
  listed entry and reported match is re-read from the real filesystem by the audit itself; where a
  later successful write or a later test run could legitimately have changed things, the check is
  relaxed to "it still exists"), evidence reused where the spec prohibits it, and events after the
  terminal state. It reads recorded observations and events, never the loop's own counters.
* **Metrics.** Lists, searches, reads, writes, tests, failed observations, recoveries, tokens and
  latency, and useful work per model call and per execution. Useful work is a verified goal: model
  claims, writes alone, a dispatched capability, stale evidence and failed test runs are not counted.
* **Exit status** (Chip's, never a native exit code): `0` verified; `1` not verified (blocked, limit,
  escalated); `2` usage; `3` infrastructure unavailable (no model selected, the selected model did not
  answer, no PAX; nothing ran); `4` runtime failure or a violated safety invariant.
* Requirements as for `verify`: a model provider behind FX and PAX 0.3.0 or later (`PAX_BIN` to
  select it); Compute is not required. PAX's own test diagnostics are shown to the model as PAX
  wrote them and can include host paths.

## Runtime service (`chip-cli serve`)

`chip-cli serve [--host ADDR] [--port PORT] [--max-concurrent-work N] [--max-queued-work N]`
(default `127.0.0.1:8765`, 2 works at once, 32 queued) exposes the same work runtime that
`chip-cli work` uses over HTTP/JSON. It contains no agent logic: both surfaces prepare a
`WorkRuntime` (model, PAX, project root) and call `WorkRuntime::run`. The model comes from
`CHIP_PROVIDER` / `CHIP_MODEL` / `CHIP_ENDPOINT` as for `work`; the project is the current directory.
If no model is selected or PAX is unusable, nothing listens (exit 3).

> **Chip Runtime Service is currently a local trusted-client interface. Remote exposure and
> authentication are intentionally out of scope.** It binds to loopback by default, has no
> authentication and no CORS, answers only loopback `Host` names when bound to loopback, and is not
> production-ready for remote or multi-user use. Work is held **in memory for the life of the
> process only**: nothing is persisted, and a restart forgets every work item.

| Route | |
| --- | --- |
| `GET /health` | `{"status":"ok"}`: service health only |
| `POST /v1/work` | `{"goal": "..."}` -> 202 `{"work_id","status":"running"\|"queued"}`; 429 `queue_full` when the queue is full |
| `GET /v1/work/{id}` | `status` (`queued`, `running`, or the terminal state), `lifecycle`, `goal`, `cancellation_requested`, `scheduling`, and once ended `result` and `timing` |
| `GET /v1/work/{id}/events` | `{"work_id","complete","events":[...]}` the runtime's recorded trajectory |
| `POST /v1/work/{id}/cancel` | queued: removed, `cancelled`. Running: `cancellation_requested` (advisory). Ended: 409 |
| `GET /v1/metrics` | service-level counts and queue/duration statistics |

The only accepted body field is `goal`. A body naming anything else (ids, receipts, observations,
evidence, executable, argv, cwd, workspace root, capability, provider, model, endpoint, priority...)
is rejected with 400 `unknown_field`. Errors are `{"error":{"code","message"}}`.

### Concurrent work

Several independent work trajectories can be active at once. This is not multi-agent collaboration:
each work has its own task, agent, goal, events, measurements and cancellation state, and nothing is
shared between works except the immutable runtime configuration. Within one work, everything stays
sequential: one decision, one capability execution, then its observation and evaluation, then the next
decision.

- **Admission** is a FIFO queue in front of `--max-concurrent-work` slots (1 to 64). Work waits as
  `queued` (no agent lifecycle yet) and starts only when a slot is free, in submission order. Up to
  `--max-queued-work` (0 to 1024) may wait; past that `POST` is refused with 429 and nothing is dropped.
- **Scheduling vs lifecycle.** `status`/`scheduling` say where the work is in admission (`queued`,
  `admitted`, `finished`). `lifecycle` is the runtime's: `null` before the work starts, `executing`
  while the loop runs, then its terminal state (`completed`, `escalated`, `blocked`, `limit_reached`,
  `failed`). `completed` is runtime completion; goal satisfaction is `result.verified`.
- **Events** are available when the work ends (the loop returns its trajectory then); a queued or
  running work reports `"complete": false` and none. Order is guaranteed within a work only, never
  across works.
- **Cancellation.** Queued work is removed and never starts (`cancelled`: no model call, execution or
  observation). Running work gets an advisory request that stops the *next* model call; a call or
  execution already in flight is not interrupted, the runtime may still complete, and the status is
  never `cancelled`.
- **Failure containment.** A panic inside one work ends that work as `failed` (no events are invented),
  frees its slot, and affects no other work, the scheduler or the server.
- **Timing.** `scheduling.queue_wait_ms` (the scheduler's), `scheduling.time_to_first_model_call_ms`,
  and `timing.model_ms` / `execution_ms` / `local_decision_ms` / `runtime_total_ms` / `turns` (the
  runtime's own measurements, copied from `result.measurement`). Observation is in-process and is not
  timed separately.
- **One workspace.** Every work runs against the single project directory the service was started
  in. The service does not clone, branch, lock or sandbox it, so concurrent work that writes the same
  files can conflict, and its results then depend on timing. Run write-heavy jobs one at a time
  (`--max-concurrent-work 1`) or in separate services over separate directories.
