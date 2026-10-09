# Chip

**Chip is the base agent. Compute is the computer.** **Rust FX supplies the model/provider boundary; Compute does not.**

Chip is an autonomous agent runtime. It uses **Rust FX** for intelligence and an **Environment** for
execution:

```text
                 Chip   (this repository)
                  |
        +---------+-----------+
        |                     |
     Rust FX             Environment
  (model/provider)      (where work runs)
        |                 |         |
      model            Local      Compute
                      machine    (hosted by an embedder)
```

Chip owns the agency: it validates every capability request, runs the work lifecycle, interprets
observations as evidence, decides whether the goal was met, and recovers within bounds. A model
supplies judgment and nothing more: its words are never an execution, an observation, evidence, a
receipt, success or completion. The environment supplies execution reality and nothing more.

## Two agent stacks, kept apart

The Compute-configured distribution already ships an npm agent. This repository is a different one:

| | Existing | This repository |
| --- | --- | --- |
| Agent | **Chip/Eve** (npm) | **Rust Chip** (`chip`) |
| Model/provider boundary | **FX/Zig** (npm/Zig) | **Rust FX** (`fx-core`, `fx-provider-http`) |
| Launched by | `compute-configured-chip` | `chip`, or `compute-configured-rust-chip` when hosted |

They are independent implementations and can coexist in one Compute-configured distribution. Neither
replaces, wraps, renames or calls the other, and Rust Chip never falls back to FX/Zig (nor the other
way round). In this document, "Chip" means Rust Chip; the npm agent is always written Chip/Eve.

## Compute is an environment, not part of Chip

```text
Rust Chip  ------->  environment contract (chip-core)  <-------  Compute (implemented by an embedder)
```

Rust Chip has no dependency on Compute and no notion of it: not in `chip-core`, not in the Rust FX
crates, not in `chip-remote-env`, not in the work/serve path. A program that wants to give Chip an
environment implements `EnvironmentProvider` and passes it to `chip_cli::service::serve_in`; Compute's
implementation lives in the Compute repository. Running Chip locally or on Compute changes where
execution happens, never what a capability means (tests show every capability byte-identical across
the two). `scripts/audit-dependencies.sh` and `crates/chip-remote-env/tests/architecture.rs` pin this.

## The executable

There is one Rust executable, `chip`:

```bash
chip --version                       # chip <version>
chip work "<goal>"                   # one bounded piece of work, on this machine
chip serve [--host H] [--port P]     # the runtime service (see below)
```

`chip work` and `chip serve` run on the local machine with no other service. A release is one
archive (`chip-<version>-<target>.tar.gz`) holding `chip` and a `manifest.json` that records the exact
Chip and Rust FX crate versions it was built from. Chip and Rust FX are versioned independently; a
consumer pins the release tag and the archive's sha256, never a moving branch.

```bash
scripts/package-release.sh dist                 # build the artifact
scripts/smoke-test.sh dist/chip-*.tar.gz        # extract it, chip --version, chip serve, a real capability, clean stop
scripts/audit-dependencies.sh                   # Rust Chip -> Rust FX; no environment provider
```

## Product boundary and crate inventory

Not every crate in this workspace is product. [`docs/product/crates.md`](docs/product/crates.md) is the
canonical inventory: what Rust Chip is and does not own, how it divides responsibility with Rust FX,
Compute, PAX, AppPort, FeltDB and Attn, and for every crate its classification (product,
integration, experiment, proof), validation level (L0 to L5), and what is still unproven. The
`chip work` / `chip serve` / `chip verify` path is the product; the graph, decision-model, Wasm and
local-model crates are experiments. A test keeps that document in step with the workspace.

What `chip work` can and cannot do today, capability by capability, with the gaps and who should own
them, is in [`docs/product/capabilities.md`](docs/product/capabilities.md). Where project
structure belongs (PAX observes; Chip decides; FX reasons) is a design contract in
[`docs/product/project-observation-boundary.md`](docs/product/project-observation-boundary.md). Its
first consumer, the `project.observe` capability (Rust structure through PAX 0.4.1 or later), is
implemented; the wider contract (the "Decision Frontier", FX reasoning over observations) is not.

How far Chip gets as a coding agent on a real project, what it recovers from, when it escalates and
what a person still has to decide is measured end to end in
[`docs/product/coding-agent-evaluation.md`](docs/product/coding-agent-evaluation.md)
(`cargo test -p chip-cli --test coding_agent`). Its judgment is scripted; its reality is not.

What ships, what is linked, what is test-only and what is experimental is mapped, with the commands
that show it, in [`docs/product/production-boundary.md`](docs/product/production-boundary.md); what
remains before Chip is a production coding agent is the prioritized backlog
[`docs/product/coding-agent-production-roadmap.md`](docs/product/coding-agent-production-roadmap.md),
and known limitations and TODO-like findings are classified in
[`docs/product/technical-debt-register.md`](docs/product/technical-debt-register.md). The normative **design** (not implemented) for how Chip should turn limited model intelligence into independently
verified work is the Work Contract, Evidence Ledger, Blockage Classifier and Micro-step Proposal Gate documents under
`docs/product/` (start at [`work-contract.md`](docs/product/work-contract.md)); the tickets are RIC-01 to RIC-07 in the
roadmap. An adversarial,
evidence-first assessment of whether Chip can be trusted to finish coding work local-first (with its
readiness judgments, findings and what could not be tested) is
[`docs/product/hostile-autonomous-agent-audit.md`](docs/product/hostile-autonomous-agent-audit.md).

Experiments on durable session storage are **not part of the product**: native FeltDB as session
memory ([`session-memory-experiment.md`](docs/product/session-memory-experiment.md); no-go) and a
comparison of FeltDB, SQLite, redb and a `durability`-crate journal against one recovery contract
([`session-store-comparison.md`](docs/product/session-store-comparison.md); decision: no durable
session store until a concrete resume requirement exists). They live in the leaf crate
`chip-session-memory`, which nothing depends on and which the shipped `chip` binary does not link.
The same comparison led to a production fix: `chip serve` now bounds the finished work it retains
(see *Retention of finished work*).

Performance is a product concern too: Chip should add minimal, bounded overhead around the model and
the computer. [`docs/product/performance.md`](docs/product/performance.md) records the measured baseline
and how to reproduce it (`cargo bench -p chip-cli --bench baseline`).

## Workspace

- `crates/fx-core`, `crates/fx-provider-http`: **Rust FX**, the provider-neutral model boundary and its HTTP provider
- `crates/chip-core`: the agent runtime, including the generic environment contract
- `crates/chip-remote-env`: a generic transport for running Chip's capabilities in an external environment
- `crates/chip-project`, `crates/chip-pax`: the project and test capabilities
- `crates/chip-cli`: the `chip` executable and the library that `serve_in` embeds

## Development

```bash
cargo check --workspace
cargo test --workspace
cargo run -p chip-cli -- --test
```

The `--test` command uses a deterministic in-memory provider and completes a single turn without external credentials or services.

## Software verification agent (`chip verify`)

```sh
cd ~/src/project
chip verify            # human-readable
chip verify --json     # machine-readable (extends the existing work measurement JSON)
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

* A model provider behind FX, configured as for the rest of `chip`: `CHIP_PROVIDER`,
  `CHIP_MODEL`, and `CHIP_ENDPOINT` / `CHIP_API_KEY` where the provider needs them.
* PAX **0.3.0 or later** for `pax.test` (`pax.execution-result.v1`) and **0.4.1 or later** for `project.observe` (`pax.observation.v1`; CI pins the release `v0.4.1`, commit `674f3b3143874d1a692aca103b33f89da31a82ac`), found as `$PAX_BIN` if set, otherwise as the
  first `pax` on `PATH`. The candidate must identify itself via `pax --version`; the POSIX `pax`
  archive utility is refused. Chip never searches for another candidate. Use `PAX_BIN=/path/to/pax`
  when `PATH` discovery is not enough.
* The native tooling PAX needs for your project (for example `cargo`).
* Not required: Compute, Attn or any other part of the wider stack.

`chip --version` prints Chip's own version only.

## Software work agent (`chip work`)

```sh
cd ~/src/project
chip work "Fix the failing tests in this project"                       # model from the environment
chip work --kind verify "Does this project pass its tests?"             # no change needed
chip work --kind inspect "Where is the request parsed? Cite the file."  # read-only
chip work --provider ollama --model qwen3-coder "Fix the failing tests"  # fully local
chip work --provider anthropic --model claude-haiku-4-5-20251001 --json "<goal>"
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
| `project.read` | `path`; optional `offset`, `length` (bytes) | reads a UTF-8 file and records its real content; a call returns at most 32 KiB, so a larger file is read in ranges (`offset`, `length` <= 32768); a range that splits a multi-byte character is refused, not altered |
| `project.write` | `path`, `content` | atomically creates or replaces a UTF-8 file (at most 32 KiB), reads it back, records what the filesystem holds |
| `project.git.status`, `project.git.diff`, `project.git.diff_stat`, `project.git.log` | none (`log`: a bounded count) | read-only Git observations; Chip fixes every argument; nothing mutating is expressible |
| `project.observe` | `scope` (required): `file:`, `path:`, `crate:` or `module:` | bounded, deterministic project structure (source files, modules, declarations, tests, manifests) observed by PAX and rendered compactly by Chip; facts, never relevance; `complete`, `partial` and failure states are explicit, and a partial observation is never shown as complete |
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
* **Goal and its kind.** The model proposes; Chip evaluates; reality provides the evidence; only Chip
  establishes completion. A model's claim of completion is a proposal and is refused until the
  observations support it. How completion is judged depends on the goal's *kind*, chosen by whoever
  submits the goal (`--kind`), never by the model and never inferred from the goal's words:
  * `change` (default): complete only when PAX established `passed` *after the last change* that
    altered a file. A pass that predates a later change does not count, and a write alone is not
    completion. The runtime completes the work itself as soon as that holds.
  * `verify`: complete when PAX established `passed` and this work changed no file ("does the project
    pass its tests as it is?"). The runtime completes it itself.
  * `inspect`: read-only. The model proposes an answer with `complete`; Chip accepts it only if
    read-only observations occurred, no file was changed, and the answer cites a file Chip observed.
    Accepted means *grounded in observation*: the run completes with `goal_satisfied: true` and
    `grounded: true`, but `verified` is false and the exit is 1. An inspection is never verified,
    because Chip does not interpret the answer.
  See [`docs/product/capabilities.md`](docs/product/capabilities.md) section 2a.
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
  latency, and useful work per model call and per execution. Useful work is a *verified* goal (`verified`, not
  merely `goal_satisfied`): model claims, writes alone, a dispatched capability, stale evidence,
  failed test runs, and an inspection that is only grounded are not counted, so an inspection
  contributes none until an independent predicate verifies it.
* **Exit status** (Chip's, never a native exit code): `0` verified; `1` not verified (blocked, limit,
  escalated, or an inspection that is answered and grounded but, as every inspection, not verified); `2` usage; `3` infrastructure unavailable (no model selected, the selected model did not
  answer, no PAX; nothing ran); `4` runtime failure or a violated safety invariant.
* Requirements as for `verify`: a model provider behind FX and PAX 0.3.0 or later (`PAX_BIN` to
  select it); Compute is not required. `chip work` only *locates* PAX at startup (a `pax` must be
  on the search path or `PAX_BIN`, else exit 3) and starts no PAX process unless the work runs
  `pax.test`; that it is PAX 0.3.0 or later is verified, once, when `pax.test` is first requested
  (an older or foreign `pax` rejects that request and the tests never run). `chip verify` verifies
  it before asking the model. PAX's own test diagnostics are shown to the model as PAX wrote them
  and can include host paths.

## Runtime service (`chip serve`)

`chip serve [--host ADDR] [--port PORT] [--max-concurrent-work N] [--max-queued-work N] [--max-retained-work N]`
(default `127.0.0.1:8765`; as many works at once as the environment can isolate, at most 2 by default, so 1 on the local machine; 32 queued; 256 finished works retained) exposes the same work runtime that
`chip work` uses over HTTP/JSON. It contains no agent logic: both surfaces prepare a
`WorkRuntime` (model, PAX, project root) and call `WorkRuntime::run`. The model comes from
`CHIP_PROVIDER` / `CHIP_MODEL` / `CHIP_ENDPOINT` as for `work`; the project is the current directory.
If no model is selected or no `pax` can be found, nothing listens (exit 3); as for `work`, PAX is
started only when a work runs `pax.test`.

> **Chip Runtime Service is currently a local trusted-client interface. Remote exposure and
> authentication are intentionally out of scope.** It binds to loopback by default, has no
> authentication and no CORS, answers only loopback `Host` names when bound to loopback, and is not
> production-ready for remote or multi-user use. Work is held **in memory for the life of the
> process only**: nothing is persisted, and a restart forgets every work item. Finished work is
> kept only up to `--max-retained-work` (see *Retention of finished work*).

| Route | |
| --- | --- |
| `GET /health` | `{"status":"ok"}`: service health only |
| `POST /v1/work` | `{"goal": "..."}` -> 202 `{"work_id","status":"running"\|"queued"}`; 429 `queue_full` when the queue is full |
| `GET /v1/work/{id}` | 410 `work_expired` if it finished and was evicted (see *Retention*); otherwise `status` (`queued`, `running`, or the terminal state), `lifecycle`, `goal`, `cancellation_requested`, `scheduling`, and once ended `result` and `timing` |
| `GET /v1/work/{id}/events` | `{"work_id","complete","events":[...]}` the runtime's recorded trajectory |
| `POST /v1/work/{id}/cancel` | queued: removed, `cancelled`. Running: `cancellation_requested` (advisory). Ended: 409 |
| `GET /v1/metrics` | service-level counts and queue/duration statistics |

The accepted body fields are `goal` and, optionally, `kind` (`change` by default, `verify` or
`inspect`; anything else is 400 `invalid_kind`). The kind says how Chip judges completion and grants
nothing. A body naming anything else (ids, receipts, observations,
evidence, executable, argv, cwd, workspace root, capability, provider, model, endpoint, priority...)
is rejected with 400 `unknown_field`. Errors are `{"error":{"code","message"}}`.

### Retention of finished work

The service keeps finished work in memory so a client can read its result and events, but only up
to `--max-retained-work N` items (1 to 1,000,000; default 256). When more finish, the **oldest
finished** work is evicted. A finished work costs roughly 25 KB (a one-turn blocked run) to 120 KB
(a long run that reads files) in the measured service, so the default bounds retained results to
tens of MiB; a caller that needs more raises the limit explicitly.

- Queued and running work is never evicted. A work that ended `escalated` is never evicted either:
  the service has no way to resolve an escalation, so it keeps them all (reported as
  `retained_escalated_work`; they do not count against the limit).
- `GET /v1/work/{id}`, `/events` and `POST /v1/work/{id}/cancel` answer **410** `work_expired` for
  an id that finished and was evicted. That response says nothing about how the work ended and is
  never a failure. The ids of evicted work are remembered, as ids only, up to four times the limit
  (at most 16,384); beyond that an old id answers 404 `work_not_found`, like an id that was never
  issued.
- A request that already holds a work when it is evicted completes normally.
- `/v1/metrics` counts stay cumulative (`submitted_work`, `completed_work`, `blocked_work`, the
  duration statistics and so on include evicted work). `retained_work`, `evicted_work`,
  `retained_escalated_work` and `max_retained_work` describe the registry itself.
- A bound on the *count* of retained items is not a ceiling on RSS: running work and the size of
  each result also contribute.

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
  `failed`). `completed` is runtime completion (Chip accepted the end of the work); `result.verified` is the independent check, and `result.grounded` says an inspection's answer is supported by observations. Only `verified` means the outcome was established.
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
- **One environment per work.** Each admitted work acquires exactly one environment, uses it for its
  whole trajectory, and releases it when it ends (also if it panics). The local machine is one mutable
  project directory, so it can be owned by one work at a time: the standalone service defaults to one
  work at a time and refuses `--max-concurrent-work N` above what the environment can isolate
  ("concurrent work requires isolated environments"). Chip does not clone, branch, lock or sandbox the
  directory to get around that. If an environment cannot be acquired the work fails with no model
  call, no execution and no observation; there is no fallback to a shared workspace.

## Architecture: the environment boundary

Chip is a standalone agent runtime. It does not own the machine or execution environment. Chip
operates against an environment boundary that may be backed by the local machine or by an external
runtime such as Compute. Compute is not a Chip dependency: Compute-configured can host Chip by
providing an environment implementation.

```
             Intelligence
                  |
                  v
              +-------+
              | Chip  |
              +---+---+
                  |
           Environment
             boundary
                  |
          +-------+-------+
          |               |
        Local          External
     environment      environment
          |               |
      machine           Compute
```

- The contract is in `chip-core` (`environment.rs`): `WorkEnvironment` (the capabilities it declares and
  executes, the observation invariants that fit it, an opaque `EnvironmentId`), `EnvironmentProvider`
  (`acquire` / `release` / `isolation_capacity`), and `Environments`, which enforces at the boundary that
  a mutable environment has at most one owning work at a time whatever a provider does.
- `chip-cli` ships one implementation, `LocalEnvironment` (`local_environment.rs`): the existing
  project, Git and `pax.test` capabilities over the current directory. `chip work` and `chip serve`
  use it by default and need no other service or network.
- An environment provider from another runtime implements `EnvironmentProvider`; the tests in
  `crates/chip-core/tests/environment.rs` state what it is held to. Chip contains no Compute client,
  configuration or types, and discovers, starts and provisions nothing.
- The model boundary (FX) is independent of the environment: neither implies the other.

## Running in an external environment (`chip-remote-env`)

*Rust Chip* is this repository's agent runtime. It is not the npm-based Chip/Eve agent that some
distributions ship; the two share nothing. Rust Chip stays standalone: `chip-core` and `chip-cli`
build, test and run on the local machine with nothing else.

`chip-remote-env` lets Rust Chip's capabilities run in an environment it does not own, through the
smallest possible transport: **`exec(argv, env)`** with captured output and no stdin
(`CommandRunner`). It does not know what provides that, and names no product.

- `RemoteEnvironment` implements the environment contract (`WorkEnvironment`) over a `CommandRunner`.
- `RemoteCapabilityBackend` is an ordinary capability backend. Rust Chip validates a request as
  always, then asks the environment to run **Rust Chip's own executor** for it:
  `chip capability-exec --root <project>` with the request in `CHIP_CAPABILITY_REQUEST`. The
  environment performs the process; what `project.write` means, the shape of its observation, how
  `pax.test`'s `pax.execution-result.v1` is read and whether the goal is met stay Rust Chip's. Tests
  show every capability is byte-identical to the local one, against real Git and real PAX/Cargo.
- A command receipt the environment records proves the environment ran something. It is never a
  Rust Chip receipt and never settles a goal.
- A request that does not fit in one environment variable (120 KiB) is refused whole, never truncated.

Another program embeds Rust Chip by implementing `EnvironmentProvider` (acquire / release /
isolation capacity) and calling `chip_cli::service::serve_in` with its `Environments`. Rust Chip
never depends on that program. The first such provider, over Compute sessions, lives in the Compute
repository (`crates/compute-rust-chip` and `docs/rust-chip.md` there; neither path exists in this
repository).
