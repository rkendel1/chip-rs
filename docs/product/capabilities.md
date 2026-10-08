# Rust Chip capability inventory and audit

The capability-level counterpart to [`crates.md`](crates.md). That document says which *crates* are
product; this one says what the product can actually *do*, what it cannot, who should own what is
missing, and what evidence each statement rests on.

This is an audit, not an expansion: no capability, crate or abstraction was added. Each claim below
is backed by a test (named where it exists) or by a reading of the code (marked as such). The
scenario tests use the real product entry point over real files, real `git`, real PAX 0.4.1 and
cargo; **only the model is scripted.** They show that the capability surface *composes*. They say
nothing about how well any real model uses it.

## 1. The answer

**Is Rust Chip an effective project/coding agent today?** For bounded jobs, yes; as a general
coding agent, no.

* **Yes, for three kinds of goal**, each judged by Chip from observations and never by the model's
  say-so (section 2a):
  * **change**, the default: "change this small project (files under 32 KiB, in directories that
    already exist) so its tests pass". Search, read, write, inspect the diff, run the tests, see the
    real failure, repair and re-run all compose, and the runtime completes the work itself when PAX
    passes after the last change (scenarios 2 and 3).
  * **verify**: "does the project, as it is, pass its tests?" Completes when PAX passes and this work
    changed no file. This is also the honest no-op: a goal that already holds is verified, not
    "fixed" through a fake write (scenario 5).
  * **inspect**: "where is X defined?" Completes when the model's answer cites a file Chip observed
    and no file was changed (scenario 1). The result is **answered and grounded, never verified**:
    exit 1, `verified: false`. Chip does not interpret an arbitrary natural-language answer, so
    nothing independent establishes that it is true (section 2a).
* **Reading is not limited by file size**: `project.read` takes an optional byte `offset` and
  `length` (at most 32 KiB per observation), so a file of any size is inspectable through bounded
  observations (G2, closed).
* **No** for: *changing* a file over 32 KiB (a write replaces the whole file and is limited to
  32 KiB: G4); creating directories, deleting or renaming; anything outside the project; and a
  guarantee that an inspection answer is right.
* **Unmeasured:** whether a real model does any of this well. No scenario here ran a real model, so
  the validation level is L3, not L4.

**The next capability, from the evidence:** none is justified yet. The audit's first two gaps (the
completion contract, G1; the read size limit, G2) are closed, and every remaining gap is either a
design question (G3, G4), someone else's boundary (G6, G7) or a policy (G8). The next step is to run
these scenarios against a real model (G10), not to add a capability.

## 2. Capability model

A capability is not a crate. `project.read` is a capability; `chip-project` is the implementation
boundary that provides eight of them. `pax.test` is a capability; `chip-pax` is its integration. A
gap in the matrix does not mean a new crate.

A capability is declared with an id, named inputs (each required or optional), and a contract; it
reports its own availability; it validates its own input values; it executes; and its result becomes
an observation. The model proposes; Chip decides:

```text
model reply
  -> ModelDecisionBoundary: strict chip.work-decision.v1; unknown capability, extra field, wrong
     type, malformed reply: rejected (no repair, no retry)
  -> the capability must be declared and available
  -> inputs: declared names only; required present; "inputs" member absent when none are declared;
     the capability's own value rules (path, size)
  -> Chip builds the ExecutionRequest and its execution id (the model supplies neither)
  -> CapabilitySet routes by declared id to exactly one backend (no fallback, no "closest")
  -> execution -> observation -> evidence -> goal evaluation
```

Verified by: `capability_scenarios.rs` scenarios 4 (unknown capability: nothing runs, nothing is
substituted, no further model call) and 4b (a declared capability asked outside its authority is
refused before it runs); `chip-core/tests/invocation_boundary.rs`, `protocol_contract.rs` and
`capability_surface.rs` (strict parsing and invocation rules); `capability_surface.rs` in `chip-cli`
(below).

What happens when a capability is missing or unavailable: the request is **rejected and the work
ends `Failed`** (exit 4, "runtime failure"); a declared capability asked outside its authority ends
`Blocked` (exit 1). Neither executes anything or substitutes. A model that sees the capability list
and chooses to stop does so with `block` or `escalate` (`inspect_requires_observation_and_forbids_change`, and a `block` with a finding). Finding: a request for a
nonexistent capability is a failed *decision*, but exits in the same class as a violated safety
invariant. That conflation is worth revisiting when exit codes are next considered.

## 2a. The completion contract

> The model proposes. Chip evaluates. Reality provides the evidence. Only Chip establishes completion.

A goal has a **kind**, chosen by whoever submits it (`chip work --kind change|verify|inspect`, or
`kind` in the service request; default `change`). It is never chosen by the model and never inferred
from the goal's words. The kind selects what Chip evaluates; it adds no capability and no authority.

| Kind | Chip's evaluation, from observations only | Who completes the work | Verified means |
| --- | --- | --- | --- |
| **change** | a content-changing write was observed, and PAX then established `passed` with no later change | the runtime, the moment that holds (no completion call by the model) | PAX passed after the last change |
| **verify** | PAX established `passed`, and this work changed no file | the runtime, the moment that holds | PAX passed on the unchanged project |
| **inspect** | at least one successful read-only observation (list, search, read, Git), no content-changing write, **and** the answer the model proposed cites at least one file Chip observed (a read's path, a search match, a listed file) | the model proposes the answer with `complete`; Chip accepts or refuses it | **nothing: an inspection is never `verified`.** The answer is `grounded` (supported by what was observed), not established as true. Exit 1 |

**Four words, kept apart.** `completed` means Chip accepted the model's proposed end of work under
the kind's completion contract; it does not mean Chip proved the outcome. `goal_satisfied` means the
kind's required condition held at the last evaluation (a level: nothing remained; not whether the
last observation produced it). `grounded` means the accepted answer is supported by observations
Chip has (an inspection only). `verified` means an independent predicate established the requested
outcome, and **only `verified` authorizes exit 0**. For change and verify, completed implies
`goal_satisfied` and `verified`. For an inspection, completed implies `goal_satisfied` and
`grounded`, and `verified` is false: `GoalKind::verified(Inspect)` is the seam where an
independently owned predicate would later establish it. Exit: completed and verified 0; completed
and grounded only 1; completed and neither 4 (a contradiction in the runtime itself).

What does not change, for every kind:

* A model's `complete` is a **proposal**. With the kind's evidence missing it is refused
  ("completion refused"), nothing executes as a side effect, and the work ends `Blocked` (exit 1).
* Freshness is by position in the recorded trajectory: a PAX pass counts only if no content-changing
  write follows it. Write, test, write again, claim: the claim is refused. A write of identical bytes
  is `changed: false` and is neither a change nor evidence.
* A claimed answer is checked against observations, never believed: it must name an observed file as
  a whole path (`mysrc/lib.rs`, `src/lib.rs2` and an unobserved `src/other.rs` do not ground an answer
  about `src/lib.rs`). A directory, a failed observation or a bare listing of another directory
  grounds nothing.
* The safety audit re-evaluates the recorded outcome independently of the loop: a completion whose
  answer the answer check would have refused is an `unauthorized_completion`.
* The rules the model is told are per kind and Chip-owned, so it knows how its work will be judged.

**`project.observe` does not ground an answer.** An observation is a fact about structure, not work, and
the completion contract is unchanged by it: `InspectionObserved` and `GroundedAnswer` count only what `project.list`,
`project.search`, `project.read` and `project.git.*` observed. An inspection that used `project.observe` alone is
refused (`completion refused`, Blocked); one that also read or searched a file it cites is grounded, as before.
Neither `partial` nor `complete` is evidence that a goal is met, `verified` and `goal_satisfied` do not read it,
and it is never useful work.

Limits stated plainly: citation is not correctness; an inspect answer can cite a real file and be
wrong, can cite a file whose relevant bytes were never observed (a search row shows one line), and
can say "there is no information" while citing files it read. Chip does not interpret the answer, so
all of these are `grounded`, none is `verified`, and only a model's own `block` ends as Blocked. Only a file whose path is cited counts, so an answer about a directory or a symbol must still
name a file. The default `change` kind still refuses a completion that changed nothing, which is
correct for change work.

Implementation: `chip-core` gained `AnswerPredicate` (a check of the proposed answer against the
observations, mirroring `ObservationPredicate`) and the runtime applies it at the same gate that
already refuses unsupported completions. The kinds, `VerifiedState`, `InspectionObserved` and
`GroundedAnswer` live in `chip-cli`'s `software_work` module. No capability, crate or authority was
added.

## 3. Current capability inventory

Machine-read by `crates/chip-cli/tests/capability_surface.rs`: the declared surface of the local
environment must equal this table exactly. Classes follow section 11 of the brief; owner is who owns
the *authority*. The level is the highest validated against the real thing; see section 13.

<!-- capabilities:start -->
| Capability | Class | Authority | Owner | Level |
| --- | --- | --- | --- | --- |
| `project.list` | CORE | read | Chip (chip-project) | L3 |
| `project.search` | CORE | read | Chip (chip-project) | L3 |
| `project.read` | CORE | read | Chip (chip-project) | L3 |
| `project.write` | CORE | write | Chip (chip-project) | L3 |
| `project.git.status` | CORE | read-git | Chip (chip-project) | L3 |
| `project.git.diff` | CORE | read-git | Chip (chip-project) | L3 |
| `project.git.diff_stat` | CORE | read-git | Chip (chip-project) | L3 |
| `project.git.log` | CORE | read-git | Chip (chip-project) | L3 |
| `pax.test` | INTEGRATION | exec-via-tooling | PAX (chip-pax) | L3 |
| `project.observe` | INTEGRATION | read-via-tooling | PAX (chip-pax) | L3 |
<!-- capabilities:end -->

Authority classes: **read** (reads files under the project root), **read-git** (runs `git` with an
argument vector Chip fixes, read-only), **write** (changes one file), **exec-via-tooling** (starts
the project's own test tooling through PAX), **read-via-tooling** (asks PAX to read the project's
manifests and Rust source and report structural facts; it starts no project command, builds nothing
and writes nothing). Three tests pin that exactly one capability writes, exactly one runs project
tooling, and exactly one is read-via-tooling, and that no declared capability is a shell, a process, a
network call, a delete, or a Git mutation.

Other things that look like capabilities but are not part of the product surface:

| What | Class | Notes |
| --- | --- | --- |
| `compute.op_a/b/c`, `compute.selftest`, `compute.hash`, `compute.system_info` (`chip-compute`) | PROOF / INTEGRATION | Offered only by the proof and benchmark commands (`--test-*`, `--horizon-matrix`), never by `chip work`, `verify` or `serve` |
| `chip.capabilities.v1` catalog, impact and slice bindings (`chip-graph`) | EXPERIMENT | Maps capabilities to graph nodes for the decision research; nothing in the work loop reads it |
| `TestExecutor`, `ScriptedPolicy`, `TestLocalReasoner` | SUPPORTING (test doubles) | Not offered to a model |
| `LocalReasoner` adapters (`chip-local-ml`, `chip-laya-reasoner`, `chip-wasm-reasoner`) | EXPERIMENT | Advise a continue/escalate verdict; they execute nothing |

## 4. Authority, side effects and failure, per capability

| Capability | Inputs the model may give | Side effects | Validation | Fails as | Reversible? | Collateral |
| --- | --- | --- | --- | --- | --- | --- |
| `project.list` | `path` (optional) | none | project-relative path rules; no `.git`, `.env*`, symlinks | `not_found` | n/a | none; at most 200 entries |
| `project.search` | `query` (literal, <=200 B), `path` (optional) | none | as above; examines <=500 files of <=256 KiB, returns <=50 matches / 16 KiB | `not_found`; skipped-file counts reported | n/a | none; **not a pattern** (`l.n` matches nothing) |
| `project.read` | `path`; optional `offset` (bytes, >= 0) and `length` (bytes, 1 to 32768), both integers | none | as above, identically for ranges; UTF-8 only; **never more than 32 KiB returned by one call**; ranges are validated before anything runs | `not_found`, `too_large` (whole-file read only), `not_utf8`, `offset_beyond_end`, `range_splits_character` | n/a | **content is sent to the model provider**; only `.git` and `.env*` are withheld, so other secret-bearing files are readable |
| `project.write` | `path`, `content` (<=32 KiB, whole file) | creates or replaces one file; atomic (temp file then rename), read back and compared | as above; parent directory must exist; no `.git`, `.env*` | `parent_missing`, `permission_denied`, `io_error` | **No capability restores the old content**; recoverable only from Git, or by writing back what the model read | any non-reserved file in the root, including `Cargo.toml`, `build.rs`, tests and CI config (`.github/...` passes the path rules; read from the code) |
| `project.git.status` | none | none | fixed `git` argv; read-only subcommands only | `not_a_repository`, `git_unavailable`, `too_large` | n/a | runs repository-configured filters like any `git status` |
| `project.git.diff` | none | none | as above | as above; fails instead of truncating at 32 KiB | n/a | whole-tree diff only; untracked files absent and said so |
| `project.git.diff_stat` | none | none | as above | as above | n/a | none |
| `project.git.log` | `count` (1 to 50) | none | as above | as above | n/a | none |
| `project.observe` | `scope` only (required): `file:<path.rs>`, `path:<prefix>`, `crate:<package>[/lib\|/<bin\|test\|bench\|example>/<name>]`, `module:<package>/lib::crate[::<module>...]`. No limit, path outside the project, command or fact. | none | the scope grammar, then project-root containment **before** PAX is started (lexical rules as the project paths; every symlink refused, inside or out); fixed `pax --dir <root> --json observe --scope <s> --max-files 50 --max-bytes 1048576 --max-facts 600`; PAX >= 0.4.1 verified once per work (the version is the contract; `observe` support is never probed); every artifact a fact names is re-walked, and a fact behind a symlink is omitted | `complete`, `partial`, `invalid_scope`, `limit_exceeded`, `artifact_unreadable`, `unsupported`, `malformed` are **observation states**, not work outcomes; no PAX, an unparseable failure or a timeout gives no observation | n/a | PAX reads <= 50 files / 1 MiB of source for one scope and starts `cargo metadata --no-deps --offline`; the model is shown <= 16 KiB, rendered by Chip (never PAX's JSON), with what did not fit counted. **Facts, never relevance**; not a completion, and it does not ground an inspect answer (section 2a) |
| `pax.test` | none | **whatever the project's tests and build scripts do**, with the tools' permissions | fixed `pax --dir <root> --json test`; PAX >= 0.3.0 verified once per work; 300 s limit | `failed`, `not_run`, `unsupported`, `ambiguous`, `error` results are observed, not retried | no | unbounded in principle: see section 9 |

**The ranged read.** `project.read` is still the one read capability, with the same path validation
(the model's path goes through the existing project-path check, then the authorized file is read; no
other path to a file exists), the same `.git`, `.env*`, symlink and root restrictions, and the same
UTF-8 rule. Two optional integer inputs bound it: `offset` (bytes, default 0) and `length` (bytes,
1 to 32768, default 32768). With neither, it is the whole-file read it always was (a file over 32 KiB
is `too_large`). The limit did not move: **one call never returns more than 32 KiB**, and a request for
more is refused at validation, before any file is touched. The result's first line adds
`offset`, `bytes` (returned), `file_bytes` (the file's size when read, from metadata, not from reading
it) and `complete` (true only when the observation starts at byte 0 and holds all of the file); the
`sha256` is the hash of the bytes returned, which for a range is the range. No hash of the whole file
is claimed, because computing one would read the whole file.

* `offset == file_bytes` is a successful empty observation; `offset > file_bytes` is the failure
  `offset_beyond_end` (with the size). Neither is `not_found`.
* A range whose start or end falls inside a multi-byte character is refused as
  `range_splits_character`, reporting `leading_partial_bytes` and `trailing_partial_bytes` so the
  caller can move the edge. The bytes are never trimmed, replaced or returned. A truncated character
  at the file's own start or end, or any invalid byte, is `not_utf8`, as for a whole-file read.
* Assembling a file from ranges is the caller's job, one context slot per range; the context budget
  bounds how much of a large file one work can hold. Nothing here changes a file.

Higher-authority capability classes, **none of which exists in the product**, each needing an
explicit design before it could: arbitrary shell, network, secrets, destructive filesystem
operations, Git mutation, external-service mutation.

## 5. Observation and evidence completeness

For every capability: request, execution, result, observation, evidence, goal evaluation.

| Need | What the product provides | Enough for autonomous recovery? |
| --- | --- | --- |
| stdout / stderr | `pax.test`: PAX's verdict on line one, `pax_process_exit`, then the native tools' stderr under a "diagnostics only; never evaluated" label (cap 256 KiB). Scenario 3 shows the model receiving the failing test's name and the `left`/`right` assertion text | Yes. Noisy (full backtraces) but present |
| exit status | `pax.test`: PAX's own `status`/`reason`/`exit_code` and the process exit; project and Git capabilities report structured success or an `error` code | Yes |
| file changes | `project.write`: `operation` (created or replaced), `bytes_written`, `sha256`, **`changed`** (false when the bytes were identical); `project.read`: `bytes`, `sha256` | Yes |
| Git changes | status (branch, staged, unstaged, untracked, deleted, clean); diff (text, `complete`, `includes_untracked`); diff_stat (insertions and deletions) | Yes for tracked files; a new file shows in status, not in the diff, and the diff says so (`git_status_shows_a_new_file_but_the_diff_does_not`) |
| PAX results | validated `pax.execution-result.v1`; malformed or mismatched results are rejected and create no observation | Yes |
| failure information | `project.*`: `not_found`, `too_large`, `not_utf8`, `parent_missing`, `permission_denied`, `io_error`; Git: `not_a_repository`, `git_unavailable`, `too_large`; failed observations are ruled out by Chip in the next request with the invocation named | Yes |
| evidence identity | execution id is Chip's: the work loop assigns `<work id>-exec-<n>` (a per-work counter) when it accepts a request; the provider's response id is optional correlation metadata (`provider_response_id`) and never an identity; content hashes on read and write; no cryptographic receipt exists (PAX issues none and Chip invents none) | Adequate. Ids are unique within a work (`(work_id, execution_id)` is the key), which is uniqueness by construction, not a cryptographic claim (`crates/chip-core/tests/execution_identity.rs`) |
| stale evidence | every product capability depends on state that changes between requests, so none is ever answered from remembered evidence (pinned by `no_declared_capability_is_a_shell...`); a PAX pass counts only if it follows the last content-changing write | Yes |

No capability was found whose result is insufficient for the recovery loop that the tests exercise.

## 6. The agent jobs

| Job | Verdict | Evidence and limits |
| --- | --- | --- |
| **A. Understand a project** | Sufficient for small projects | `list`, `search` (literal), `read`, Git status/diff/log and `pax.test` answer structure, content, state and test reality (scenarios 1, 2). Limits: 32 KiB per read observation (a larger file is read in ranges), 200 entries per listing, 500 files searched, no pattern search. The result is reported as a completed answer with `--kind inspect` (section 2a) |
| **B. Modify a project** | Sufficient for a basic coding loop | search, read, write, diff, status and `pax.test` (scenario 2). Limits: whole-file replace only, so a file over 32 KiB can be read in ranges but not changed (G4); the directory must exist; no create-directory, delete or rename (G3) |
| **C. Verify its own work** | Yes, within what PAX proves | `pax.test` passing after the last content-changing write, decided by Chip from PAX's `status`, not from a model claim (scenarios 2, 3, 5); verify-only goals complete without any write (`--kind verify`). **PAX proves** that the project's own test operation ran and what it reported. **It does not prove** that the tests are adequate, that the change is minimal or correct beyond them, that lint, format or type checks pass, or that the stated goal means what the tests check. Git status/diff show what changed, not that it is right |
| **D. Recover from failure** | Yes, with the existing capabilities | observe the failing `pax.test` (verdict and diagnostics), write a fix, re-run, compare (scenario 3). No new capability was needed. Bounded by the work limits (default 12 turns / 8 executions) |
| **E. Work with Git** | Read: yes. Mutation: not needed for this product | Inspect state, changes, history and the resulting diff (scenario 2). The work contract ends at "tests pass in the working tree"; a human reviews the diff. See section 10 |
| **F. Work with external systems** | Not required for this product | See section 11 |

## 7. Composition scenarios

`crates/chip-cli/tests/capability_scenarios.rs`; each asserts what reality did.

| # | Scenario | Result |
| --- | --- | --- |
| 1 | Inspect: list, search, read, then report | As **inspect** work: the three observations are real; the answer cites `src/lib.rs`, Chip accepts it, the work **Completes** as answered and grounded (`verified: false`, exit 1), nothing on disk changed, no test was run. An answer that cites nothing, an unobserved file, or part of a longer path is refused, as is a claim with no observation or after a change. As default **change** work the same claim is still refused (a change goal needs a change) |
| 2 | Modify: search, read, write, diff, verify | **Completed**, verified. The model sees the real diff (`+pub fn canonical`). The runtime completes the work itself the moment PAX reports `passed` after the change; the model makes no completion call |
| 3 | Repair: write a wrong fix, test fails, read the evidence, write the right fix, test passes | **Completed**, verified, one recovery. The model's second-turn input contains PAX's `failed` status, the failing test's name and the assertion text, and Chip's own "ruled out: pax.test: its execution failed" |
| 4 | Block: ask for `shell.exec`, `project.delete`, `git.commit`, `http.get` | Each is rejected as an unknown capability; the work ends `Failed`; **only the earlier read-only step ran**, no further model call, nothing substituted, nothing on disk changed |
| 4b | A declared capability outside its authority: write into `.git`, outside the root, an absolute path, `.env`; read outside the root | Each ends `Blocked` ("invalid capability input") with **zero executions** and no file created |
| 5 | No-op: the goal already holds | As **verify** work: `pax.test` passes on the unchanged project and the runtime **Completes** it with zero writes. As **change** work it is still refused, and rewriting identical bytes is no shortcut (`changed: false` is not a change). A failing project under verify does not complete, whatever the model claims afterwards |
| - | A pass that predates a change | Change work: write, test (passes), write again, claim: refused; re-tested after the last write: accepted. Verify work: pass, then a change, then a claim: refused. (Tests make the model the one that asks to finish, since Chip would otherwise complete at the first pass) |
| - | The model cannot manufacture completion | In every kind, "complete" with no evidence is refused: zero executions, no retry, no change, a clean audit |
| - | No kind widens the boundary | A request for `shell.exec` fails closed in every kind |
| - | Large files | A whole-file read of a 40 KiB file is `too_large`; `offset`/`length` read it in bounded ranges (section 4). `a_file_larger_than_the_read_limit_is_inspectable_through_bounded_observations` reads this repository's 102 KB work loop in parts, shows the model a marker that lies past the first 32 KiB, and completes an inspection that cites it |
| - | Limits are observed, not hidden |  non-UTF-8 as `not_utf8`; a new file in a missing directory as `parent_missing`; 40 KiB of content, or a path with a space, is refused before anything runs |
| - | Authority through tooling | A model that writes a test and runs `pax.test` makes the project's tooling execute its code, which here writes a file outside the project root. This is the documented limit of the model (README), recorded by `write_plus_test_is_code_execution_through_the_projects_own_tooling` so that a future sandbox changes the test |

## 8. Gaps, from the scenarios

Each gap names the concrete use case that fails, why the current surface cannot do it, who should
own it, the authority it would carry, and the evidence. Priority is for the first real product.

| # | Gap | Use case that fails | Why current capabilities cannot | Owner | Authority | Evidence | Priority |
| --- | --- | --- | --- | --- | --- | --- | --- |
| ~~G1~~ | ~~No way to complete a non-mutating goal~~ **Closed** by the goal kinds (section 2a) | "Where is X defined?", "does the build pass?", "is it already fixed?" | Was: completion required a content-changing write followed by `passed` | Chip (goal evaluation) | none | the inspect, verify and no-op tests in `capability_scenarios.rs` | Done. **Residual (G1b): an accepted inspect answer is grounded, never verified**: it exits 1 and `verified` is false. A real-model run showed a grounded answer can be a non-answer, so verification needs an independently owned predicate; none exists yet |
| ~~G2~~ | ~~No ranged read; files over 32 KiB are unreadable~~ **Closed for reading** by the ranged `project.read` (section 4) | Inspect any real codebase's larger files | Was: `project.read` returned the whole file or `too_large` | Chip (chip-project) | none beyond `read` | `a_file_larger_than_the_read_limit_is_inspectable_through_bounded_observations` (this repository's 102 KB `work.rs` read in ranges) and the `chip-project` range tests | Done. **Residual:** reading a big file costs one model-context slot per range (the context budget bounds it, nothing assembles ranges for the model), and the write half (changing a large file) is G4 |
| **G3** | **Cannot create a directory, delete or rename a file** | Add a module in a new directory; remove dead code | `project.write` needs an existing parent; no other mutation exists | **Chip** (chip-project) | destructive for delete and rename: needs a design (for example, only inside a clean Git tree, so it is recoverable) | `parent_missing` observation; surface test | Medium |
| **G4** | **Whole-file replace is the only mutation, and is limited to 32 KiB** | Change one line of a 30 KiB file; change any line of a larger file | The model must re-emit the whole file, risking silent loss; mitigated by the write's read-back hash and `git diff`, not prevented. **A file over 32 KiB can now be read but still cannot be changed** | **Chip** (chip-project) | as `write` | design reading; scenario 2 shows the diff catches changes; `oversized_content_and_malformed_inputs_are_refused` | Medium; a partial-edit design is its own PR |
| **G5** | `project.git.diff` has no path scope, fails over 32 KiB, omits untracked files | A big working tree; new files | One whole-tree diff; but `status` lists the new file and `read` shows it | Chip (chip-project) | none | `git_status_shows_a_new_file_but_the_diff_does_not` | Low: composes around it |
| **G6** | **Project tooling is not sandboxed**: `write` + `pax.test` is code execution | Any untrusted or remote use | The boundary is on files the model may touch, not on what the tests do | **Compute / the environment** (isolation), not a new Chip capability | the largest authority in the product | `write_plus_test_is_code_execution...` | **High before untrusted or hosted use**; acceptable for a local trusted developer, as the README states |
| **G7** | Only the `test` operation is verifiable | Lint, format or type-check the change | PAX is invoked for `test` only | **PAX** (new operations); Chip would consume them as capabilities | as `pax.test` | no failing scenario: **the need is not yet evidenced** | Defer until a case shows it |
| **G8** | **File contents are sent to the model provider; only `.git` and `.env*` are withheld** | Private repositories with a hosted model | Path rules are a deny-list of two names | **Chip** (path policy) and the operator's choice of provider; **Attn** for human decisions | confidentiality | path rules (code) | Medium; matters before private-repo use |
| G9 | ~~Execution ids not unique if a provider repeats response ids~~ | Local servers with constant ids | Closed: Chip assigns the id (`<work id>-exec-<n>`); the provider's response id is metadata only | Chip | none | `execution_identity` tests | Closed |
| G10 | No real-model run of these scenarios | Knowing whether the product is *effective* | All scenarios use a scripted model | Chip (validation) | none | this document | **High** for the claim; it is the next measurement |

## 9. Shell and process execution

**Does Chip need arbitrary process execution?** Not for the first product. Work that needs a process
and how it is served:

| Need | Served by | Needs `shell.exec`? |
| --- | --- | --- |
| Run the tests / build | `pax.test` (PAX runs the native tool) | No |
| Inspect Git | fixed-argv `project.git.*` | No |
| Format, lint, type-check | not served (G7) | No: ask PAX for the operation |
| Install dependencies, provision tools | the environment / Compute | No: not an agent act |
| Run the application, a dev server, a one-off script | not served | Evidence absent; it would be a Compute capability with its own authorization |

The narrower model holds. The honest caveat is G6: because the model can write code and PAX runs the
project's tests, **Chip already has indirect arbitrary execution**, bounded by the files the model may
touch and not by what the tests do. Adding `shell.exec` would add *direct* execution with no new
need to justify it; it is not added, and the surface test fails if a capability named like one
appears. If a real case needs it, it belongs to Compute (the computer), authorized per use.

## 10. Git

| Operation | Classification | Reason |
| --- | --- | --- |
| status, diff, diff_stat, log | **required / useful, present** | Needed to inspect state and verify the change |
| add / stage | unnecessary (alone) | Only matters with commit |
| commit | **useful later**, not required | The contract ends at "tests pass in the working tree"; commit needs authorship, message and a human decision. Candidate for Chip/project behind explicit authorization (and Attn) once the work contract includes it |
| branch | unnecessary | Isolation is the environment's job (one environment per work) |
| checkout | dangerous | Discards or moves working-tree state; would also be the only "revert" |
| reset | dangerous | Irreversible loss of uncommitted work |
| push | belongs outside Chip | Network, credentials and publication; a human or CI decision |
| stash, merge, rebase, clean | dangerous / unnecessary | Rewrite or discard state |

## 11. External systems

None is required for the first product. Each, for a later one:

| System | Generic or application-specific | Owner | Boundary it would need |
| --- | --- | --- | --- |
| HTTP / API | generic transport, application-specific meaning | **AppPort** (explicit, per-service capabilities) | allow-listed hosts and methods, no model-chosen URLs, secrets held by the port, response size bounds |
| Database query | application-specific | **AppPort** | read-only by default, parameterised, per-database credentials held outside Chip |
| Browser | generic | **AppPort** (a browser service) or Compute (a session) | origin allow-list, no ambient credentials, isolated profile |
| Shell / process | generic | **Compute** | section 9 |
| Artifacts (files outside the project) | generic | **AppPort** or the environment | separate roots, never a model-supplied absolute path |
| Cloud / service mutation | application-specific | **AppPort**, with **Attn** for approval | explicit, auditable, reversible where possible |
| Secrets | cross-cutting | the **environment / AppPort** hold them; Chip never sees them | never in model context |
| Messaging | application-specific | **AppPort**; **Attn** for human-facing decisions | outbound only, rate-limited, approval for anything irreversible |

## 12. Ownership

| Owner | Capabilities and concerns |
| --- | --- |
| **Rust Chip** | deciding to invoke an authorized capability; invocation validation; bounded progression; failure and recovery; escalation; goal evaluation; the project file and Git-read capabilities as implemented in `chip-project` (G2, G3, G4, G5, G8, G9) |
| **PAX** | project test interpretation and structured evidence; new verification operations (G7) |
| **Compute** | the computer: processes, sessions, isolation and sandboxing (G6), provisioning |
| **AppPort** | explicit application and service capabilities: HTTP, databases, browsers, messaging |
| **Attn** | human authority and decisions: approvals for commits, publication, anything irreversible |

## 13. Validation

Every product capability is **L3**: exercised against the real dependency (real filesystem, real
`git`, real PAX and cargo) by `chip-project`'s and `chip-pax`'s tests and by the scenarios above,
with a scripted model. **None is L4**: no real-model run of these scenarios is recorded. The
opt-in real-model tests in `chip-cli/tests` have not been run for this audit. A product-grade claim
for any capability stops at L3 until they are.

## 14. Deliberately excluded

Shell or process execution; network and HTTP; browser; database; secrets; messaging; cloud or service
mutation; Git mutation (commit, branch, checkout, reset, push); delete, rename and create-directory
(not excluded forever: G3, awaiting a design); file access outside the project root; any
model-supplied command, executable, argument vector, working directory, root, status, observation or
receipt.

## 15. Follow-up PRs, in order of evidence

1. **Run the scenarios against a real model (G10).** It decides whether the rest matters. Measure
   with the performance harness's L2 tier; record valid and invalid decisions, recoveries and
   verified goals per model call.
2. **A partial-edit design (G4)**, only if real-model runs show whole-file replacement losing content or
   large files needing changes. Not before the L2 runs.
3. **A design for create-directory / delete / rename (G3)**, with the recoverability condition decided
   first.
4. **Sandbox the project tooling (G6)**, in Compute or the environment, before any untrusted or
   hosted use.
5. **Path and secrecy policy for what is sent to a provider (G8)**, before private repositories.

Not on the list, because no evidence supports them: a shell, HTTP, a browser, a database, Git
mutation, new PAX operations.
