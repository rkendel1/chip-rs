# Coding-agent production roadmap

**The canonical backlog** for moving Rust Chip from what it does today to a production-grade
autonomous coding agent. Other documents say what *exists* ([`crates.md`](crates.md),
[`capabilities.md`](capabilities.md), [`production-boundary.md`](production-boundary.md)); this one
says what *remains*, in what order, and how each item is proven done. Items that were previously
only gaps in those documents (`capabilities.md` G1 to G10, the limits in
[`coding-agent-evaluation.md`](coding-agent-evaluation.md) section 6, `crates.md` section 8) are
folded in here with their ids so nothing is tracked in two places.
[`technical-debt-register.md`](technical-debt-register.md) classifies every TODO-like finding and
points here for the ones that are real work.

**Clean repository is not production readiness.** Nothing below is a claim that Chip is production
ready, and completing this list would not by itself be one.

## How to read it

* **Priority** is a proposed planning class, not a promise of order within a release: **P0** wrong
  or unsafe results; **P1** needed for reliable autonomous work; **P2** quality, independence and
  release discipline; **P3** deferred ideas with no current requirement.
* **Status** is validated against the repository on 2026-10-09: *implemented* (code and tests
  establish it for the stated scope), *partial*, *missing*, *blocked* (needs a decision or an
  external owner first), *not yet validated* (code exists; no evidence it works under real
  conditions).
* **Evidence** names a test, a document section or a call site. "Read" means found by reading the
  code, not pinned by a test.
* Nothing in P3 enters the plan without a concrete requirement, a defined contract and measurable
  acceptance criteria.
* Standing constraints (`AGENTS.md`, `crates.md`): the model proposes and never establishes reality;
  `chip-core` stays free of Compute, FeltDB and AppPort; no shell, network, Git mutation or
  secrets capability without a separate design; PAX owns test interpretation and Chip consumes it.
  Where an item says "scripted", the evidence says nothing about any real model.

Summary of the state (details in each item):

| Area | Implemented | Partial | Missing / blocked / unvalidated |
| --- | --- | --- | --- |
| P0 correctness and safe execution | P0-02, P0-05, P0-07 | P0-01, P0-03, P0-04, P0-06 | P0-08 |
| P1 lifecycle | | P1-01, P1-03, P1-05, P1-06, P1-08 | P1-02, P1-04, P1-07 |
| P1 verification and evidence | P1-V1, P1-V5 | P1-V3, P1-V4, P1-V6 | P1-V2 |
| P1 operational reliability | P1-O3 | P1-O1, P1-O2, P1-O4, P1-O5, P1-O6 | |
| P2 provider independence | P2-01 | P2-02 | P2-03, P2-04 (documentation only), P2-05 |
| P2 developer experience and release | | P2-D1, P2-D2, P2-D3, P2-D4, P2-D5 | |
| P2 capability completeness (evidence-gated) | | | P2-C1 to P2-C5 |
| P3 deferred | | | P3-01 to P3-06 |

---

## P0: correctness and safe execution

### P0-01 Execution completion is not goal satisfaction
* **Status:** partial.
* **Evidence:** `completed`, `goal_satisfied`, `grounded` and `verified` are distinct and only
  `verified` authorizes exit 0 (`capabilities.md` section 2a; `capability_scenarios.rs`;
  `chip-core/tests/work_loop.rs`). Residuals: an accepted `inspect` answer is `grounded`, never
  `verified` (G1b); the `change` completion rule cannot express feature acceptance: the evaluation
  completed after fixing one defect while the assignment was untouched (`coding-agent-evaluation.md`
  section 6, limit 1).
* **Prevents:** reporting "done" when the requested outcome was not established.
* **Acceptance:** a goal carries an acceptance predicate supplied from outside the model (not
  inferred from the goal text) and `verified` is true only when it holds; for work without one, the
  result says `verified: false` with the reason. Existing kinds keep their current meaning.
* **Depends on / constraints:** where acceptance comes from (the project's own checks via PAX, or the
  submitter) is a design decision; Chip evaluates, it does not interpret natural language.
* **Verification:** scripted scenarios where the defect is fixed and the assignment is not (must not
  verify); a real-model run (P2-05); the falsification list in `coding-agent-evaluation.md` section 7.

### P0-02 Success rests on independently verifiable outcomes, not the model's claims
* **Status:** implemented for `change` and `verify`; not for `inspect`.
* **Evidence:** completion needs PAX `status: passed` after the last content-changing write; a
  `complete` with no evidence is refused with zero executions (`capability_scenarios.rs` scenarios 2, 3, 5
  and the "model cannot manufacture completion" tests); the safety audit re-evaluates the recorded
  outcome (`unauthorized_completion`).
* **Prevents:** a hallucinated success.
* **Acceptance:** unchanged; extend to `inspect` through an independently owned predicate (P2-C5).
* **Depends on:** PAX for `change`/`verify`.
* **Verification:** existing tests plus P0-01.

### P0-03 Preserve active work, pending escalations, decisions and necessary evidence
* **Status:** partial.
* **Evidence:** `chip serve` never evicts queued or running work, and never a work that ended
  `escalated` (`service.rs` tests `queued_and_running_work_is_never_evicted`,
  `work_that_ended_escalated_is_never_evicted`; measured plateau in
  `service-retention-bounded.json`). Gaps: nothing persists across a restart (by decision, P3-01);
  an escalated work is over and cannot be resumed in-loop (evaluation limit 3); there is no channel
  for a human decision in `chip work`/`serve`; evidence for an *evicted* finished work is gone
  (410 `work_expired`).
* **Prevents:** losing a result a person still needs.
* **Acceptance:** a decision/approval record is addressable and survives for as long as it is
  pending; the set of states that pin a work in memory is documented and tested; a client can tell
  "expired" from "failed".
* **Depends on:** P1-02 (resume contract), P1-07 (human channel). **Constraint:** no durable storage
  without P3-01's conditions.
* **Verification:** the existing service tests plus a pending-decision scenario that outlives the
  retention limit.

### P0-04 Cancellation, retries, timeouts and failures have unambiguous semantics
* **Status:** partial.
* **Evidence:** cancelling queued work removes it without a model call; cancelling running work is
  advisory and in-flight calls are not interrupted (`service.rs`; `crates.md` section 5.2). The model
  boundary has no repair and no retry by design. Timeouts: provider requests default to 30 s
  (`fx-provider-http`), `pax.test` to 300 s; limits are turns and executions (default 12/8 for
  `chip work`/`serve`, ceiling 50). No wall-clock deadline for a whole work was found (read).
  Failures end `Failed` (exit 4) or `Blocked` (exit 1); a request for a nonexistent capability exits in
  the same class as a safety-invariant violation (`capabilities.md` section 2).
* **Prevents:** work that never ends, retries that repeat a harmful action, and failures that look
  alike.
* **Acceptance:** a documented table of every terminal cause with its state and exit code; a
  per-work wall-clock bound; cancellation behaviour for in-flight model calls and executions stated
  and tested; a distinct class for "the model asked for something that does not exist".
* **Depends on:** none. **Constraint:** exit-code changes are breaking and need approval.
* **Verification:** timeout and cancellation tests against slow mock model and slow PAX shim.

### P0-05 Verification cannot be bypassed by the model's own assertions
* **Status:** implemented.
* **Evidence:** `ModelDecisionBoundary` strict parse; completion refused without the kind's
  evidence; freshness by trajectory position; capability set pinned (no shell, no process); tests
  `no_declared_capability_is_a_shell...` and the boundary tests (`capabilities.md` sections 2, 2a, 3).
* **Acceptance / verification:** keep the tests green; any new capability must extend the surface
  test.

### P0-06 Destructive actions, permissions, secrets and untrusted repository content
* **Status:** partial.
* **Evidence:** no delete, rename, Git mutation, shell or network capability exists (surface test);
  `.git` and `.env*` are never readable or writable; writes are atomic and read back. Not
  addressed: project tooling is not sandboxed, so `project.write` plus `pax.test` is code execution
  (G6; `write_plus_test_is_code_execution_through_the_projects_own_tooling`); file contents go to the
  provider and only two names are withheld (G8); a project root changed by another process can race
  the path checks; **repository content is untrusted input to the model and no document or test
  addresses prompt injection through it** (read: no mention in the docs).
* **Prevents:** data exfiltration to a provider, execution of hostile project code on the operator's
  machine, a model steered by content it read.
* **Acceptance:** execution of the project's tooling happens only in an isolated environment before
  any untrusted or hosted use (Compute or another provider owns isolation); a documented, configurable
  path/secret policy for what may be sent to a provider; a stated threat model for untrusted
  repository content with at least one test where a file's text tries to issue a decision.
* **Depends on:** an isolating `EnvironmentProvider` (not in this repository); **blocked** on that for
  sandboxing. **Constraint:** Chip does not become a sandbox.
* **Verification:** isolation tests in the provider's repository; path-policy tests; a scripted
  injection scenario showing the boundary refuses it.

### P0-07 Test and verification failures cannot be silently converted into success
* **Status:** implemented (within what PAX reports).
* **Evidence:** the verdict is PAX's `status`, never an exit code or output text; `failed`,
  `not_run`, `unsupported`, `ambiguous`, `error` are observed and end `Blocked`; a malformed or
  mismatched result creates no observation (`chip-pax` tests; `capability_scenarios.rs` 3, 5).
  Limit: a red `cargo test` stops at the first failing binary, so counts are partial
  (`coding-agent-evaluation.md` limit 7; owner PAX).
* **Acceptance / verification:** keep tests; track the PAX limit under P1-V2.

### P0-08 What the system guarantees when execution stops unexpectedly
* **Status:** missing (only fragments stated).
* **Evidence:** a write is atomic via temp file and rename but a crash can leave a
  `.chip-write-*.tmp` file (`chip-project/src/lib.rs`); a killed `chip serve` forgets all work;
  the experiments measured process-kill (not power-loss) recovery for candidate stores only
  (`session-store-comparison.md`), none of it in the product.
* **Prevents:** an operator not knowing whether the working tree, the temp files or the work record is
  trustworthy after a crash.
* **Acceptance:** a written crash contract per surface (`work`, `serve`, `verify`): what is on disk,
  what is lost, what an operator must do; leftover temp files are detected and reported at start;
  a SIGKILL test of `chip work` mid-write showing the contract holds.
* **Depends on:** none (this is a statement plus tests, not storage). **Constraint:** do not add
  persistence to meet it.
* **Verification:** kill tests of the real binary at write and between execution and recording.

---

## P1: autonomous work lifecycle

### P1-01 Work state, attempts, checkpoints and terminal outcomes are explicit
* **Status:** partial.
* **Evidence:** terminal states `Completed|Escalated|Blocked|LimitReached|Failed` plus the service's
  `cancelled`; `WorkEvent`s per work; scheduling phase `queued|running|finished`. There is no
  "attempt" or "checkpoint" concept in the product; the ladder, attempts and handoffs exist only in the
  evaluation harness (`crates/chip-cli/tests/coding_agent/`).
* **Acceptance:** an attempt has an identity above a work (a run in a ladder), recorded in events;
  checkpoints are defined as a state a new attempt can start from (not a storage format).
* **Depends on:** P1-02, P1-04. **Constraint:** no database; in-memory first.
* **Verification:** ladder scenarios promoted from the harness into a product-level test.

### P1-02 Resume interrupted work only under a defined recovery contract
* **Status:** missing, by decision.
* **Evidence:** an escalated run is over; continuing is a new run whose only memory is a handoff
  (evaluation limit 3). Process-restart recovery was assessed and deferred (P3-01).
* **Prevents:** resuming into a state the system cannot vouch for.
* **Acceptance:** a contract stating which states are resumable, what is re-verified before continuing
  (the working tree is checked against the last recorded observation), and what must escalate.
  In-process resume after a human decision first; restart recovery only via P3-01.
* **Depends on:** P1-01, P1-03, P1-07. **Verification:** resume scenario with a changed working tree
  (must refuse or re-verify).

### P1-03 A new attempt understands what was tried, failed, succeeded, and why
* **Status:** partial.
* **Evidence:** the product escalates with a deduplicated context (`DeduplicatedEscalationContext`);
  the evaluation harness builds a richer, assessed handoff (`packet.rs`) that is **test-only**; the
  decision protocol has no plan or hypothesis field (limit 6); a handoff does not fit the 2,000-byte
  goal (limit 4); file contents do not cross attempts (limit 5).
* **Acceptance:** the handoff structure is product code with a size bound that fits the surface;
  claims are labelled as assertions, facts as observations.
* **Verification:** the handoff negative tests (`coding_agent`) run against the product type.

### P1-04 Repair budgets; no endless retry or escalation loops
* **Status:** missing in the product (present in the evaluation harness only).
* **Evidence:** `chip work` has turn and execution limits and no repair budget or repeat limit
  (limit 3; `policy.rs` in the harness).
* **Prevents:** unbounded model spend on the same failure.
* **Acceptance:** repair budget and repeat-failure limit as a `LocalWorkPolicy` in the product, tested
  to stop on a repeated identical failure; budget reported in the result.
* **Verification:** scripted repeat-failure scenario; limits visible in JSON.

### P1-05 Bounded context construction and evidence selection
* **Status:** partial.
* **Evidence:** `CHIP_CONTEXT_BUDGET_BYTES` / `--context-budget-bytes` and a `ContextReport` with
  omissions (`software_work.rs`; `chip-core/tests/context_discipline.rs`); product capabilities are
  evidence-reuse-prohibited, so context is rebuilt from fresh observations. Unmeasured: how a real
  model copes with truncation.
* **Acceptance:** documented selection rules; a test that the budget is never exceeded and that an
  omitted observation is disclosed to the model.
* **Verification:** context-discipline tests plus a real-model run (P2-05).

### P1-06 Completed, incomplete, blocked and human-required are distinguishable
* **Status:** partial.
* **Evidence:** the terminal states and `status` field distinguish completed, blocked,
  limit-reached, failed, cancelled; `escalated` is the "human required" state. There is no channel
  that delivers or answers it (P1-07).
* **Acceptance:** a state-to-meaning table in the public docs and the JSON, covering the escalation
  recipient.
* **Verification:** schema/doc consistency test.

### P1-07 Escalation paths: cheaper model, stronger model, tools, humans
* **Status:** missing in the product; scripted in the harness.
* **Evidence:** `chip work` has one tier and no human channel; the ladder is
  `coding_agent/ladder.rs` with a scripted model (judgment scripted, reality real).
* **Prevents:** a hard task ending at the first model's limit with no path forward.
* **Acceptance:** a configurable ladder (`provider selection per tier`), a stop rule that does not
  assume a stronger model succeeds, and an addressable human-decision record (P0-03).
* **Depends on:** P1-04, P2-03. **Verification:** real-model runs per tier (P2-05).

### P1-08 Each escalation carries evidence, prior attempts, constraints and remaining uncertainty
* **Status:** partial.
* **Evidence:** as P1-03; the harness packet assesses completeness and refuses a handoff that presents
  a claim as fact.
* **Acceptance / verification:** as P1-03, plus a test that an escalation lacking any of the four is
  refused.

---

## P1: verification and evidence

### P1-V1 Verify changes independently of the model
* **Status:** implemented for tests; not for lint, format or type checks (G7).
* **Evidence:** `pax.test` and the completion rules (P0-02). `capabilities.md` G7: no failing scenario
  yet shows the need.
* **Acceptance:** unchanged until a case shows the need; then PAX gains the operation first.
* **Verification:** n/a until then.

### P1-V2 Test integrity, discovery, partial runs and failed-verification semantics
* **Status:** missing.
* **Evidence:** the runtime cannot see test tampering: weakened, deleted or ignored tests leave PAX
  green and the work verifies (limit 2); PAX counts are partial when red (limit 7); discovery is
  PAX's.
* **Prevents:** a "green" run achieved by editing the tests.
* **Acceptance:** a baseline comparison of the test set (existing tests only) as a runtime check, and a
  documented statement of what it cannot detect (a model's edits to its own new tests); PAX's
  partial-count behaviour fixed upstream or surfaced in the result.
* **Depends on:** PAX (owner of per-test results), P0-01. **Verification:** tamper scenarios
  (`coding_agent/integrity.rs` is the prototype).

### P1-V3 Trustworthy build, test, binary-acceptance and repository-integrity checks
* **Status:** partial.
* **Evidence:** CI runs fmt, check, test, the wasm build, the packaged smoke test and the
  dependency audit (`.github/workflows/rust.yml`, `release.yml`); the harness checks the finished
  program with cargo and the built binary outside the model loop. Acceptance beyond PAX is harness-only.
* **Acceptance:** the acceptance checks the harness uses are expressible as project-supplied
  verification (P0-01). **Verification:** CI job running the harness.

### P1-V4 Preserve receipts and evidence that explain acceptance or rejection
* **Status:** partial.
* **Evidence:** observations, content hashes and per-work events; execution ids
  `<work id>-exec-<n>`; no cryptographic receipt (PAX issues none; Chip invents none).
  **Retention trade-off introduced by the bounded-retention fix:** the explanation of a finished work
  is available only while the work is retained; after eviction the service answers 410 and the
  evidence is gone.
* **Acceptance:** the client contract for fetching results before expiry is documented; if evidence must
  outlive retention, that is a requirement for P3-01, not an implicit unbounded map.
* **Verification:** documentation review; a test that an expired work never fabricates a result (exists:
  `finished_work_is_bounded_and_the_oldest_is_evicted_first`).

### P1-V5 Integrate PAX only for capabilities it implements; document missing ones
* **Status:** implemented, ongoing.
* **Evidence:** `pax.test` (PAX >= 0.3.0) and `project.observe` (PAX >= 0.4.1) with version checks and
  strict parsing; lint/format/type operations are documented as missing (G7), not simulated.
  The `project.observe` benchmark gate (keep, narrow or remove on measured results) has no result in
  this branch: `docs/product/observe-benchmark.md` exists only on `origin/pax-observe-consumer`.
* **Acceptance:** the observe gate result is merged or the capability's status is re-stated.
* **Verification:** the matched baseline/treatment run is reproducible from the repository.

### P1-V6 Verification evidence is tied to the specific attempt and final result
* **Status:** partial.
* **Evidence:** freshness is by trajectory position within a work; an attempt above a work does not
  exist (P1-01), so evidence cannot yet be tied to an attempt identity.
* **Acceptance:** result JSON names the evidence that supports `verified` (execution id of the
  deciding `pax.test`). **Verification:** schema test.

---

## P1: operational reliability

### P1-O1 Bound completed-work retention; treat escalations separately
* **Status:** implemented for completed work; partial for escalations.
* **Evidence:** `--max-retained-work` (default 256); oldest finished evicted; 410 `work_expired`;
  cumulative metrics; tests in `service.rs` and `tests/serve.rs`; measured plateau
  (`service-retention-bounded.json`). Escalated works are never evicted and are unbounded until a
  resolution mechanism exists.
* **Acceptance for the remainder:** escalations leave memory only when resolved or explicitly expired
  by policy (P0-03, P1-07). **Verification:** extend the retention bench with a mixed escalated load.

### P1-O2 Sustained-service memory, including active work and large payloads
* **Status:** partial.
* **Evidence:** retained *count* plateaus exactly at the limit; RSS flattens but creeps by a few KiB per
  hundred works after it (`service-retention-bounded.json`; `session-store-comparison.md` section 2).
  Not measured: many concurrent active works, `pax.test` output near the 256 KiB cap, hours-long runs.
* **Acceptance:** a soak test with concurrent active work and large payloads and a stated RSS envelope.
* **Verification:** extend `benches/service_retention.rs`; run in a release gate (P2-D5).

### P1-O3 Observable service health, queue behaviour, concurrency limits, backpressure
* **Status:** implemented at a basic level.
* **Evidence:** `GET /health` (liveness only), `GET /v1/metrics` (cumulative counts, queue length,
  retained/evicted), FIFO admission, 429 `queue_full`, concurrency limits bounded by environment
  isolation (`service.rs` tests).
* **Gap:** `/health` says nothing about the model or PAX being usable. **Acceptance:** a readiness
  signal that does. **Verification:** test with an unavailable provider.

### P1-O4 Deterministic failure reporting and useful diagnostics
* **Status:** partial.
* **Evidence:** exit codes 0 to 4, JSON errors `{"error":{"code","message"}}`, structured results.
  Gap: error-class conflation (P0-04); PAX diagnostics can include host paths shown to the model.
* **Acceptance / verification:** as P0-04; a path-redaction decision for diagnostics.

### P1-O5 Supported platforms, installation paths and environment requirements
* **Status:** partial.
* **Evidence:** README states PAX/cargo/git requirements; one release target (Linux x86_64); other
  platforms untested here. **Acceptance:** a "Supported platforms" section listing exactly what CI
  establishes. **Verification:** CI matrix or an explicit "unsupported" list.

### P1-O6 Separate tested guarantees from previews and unverified configurations
* **Status:** partial.
* **Evidence:** `crates.md` validation levels (L0 to L5) and "Not yet proven"; README gives
  limits. **Acceptance:** README links the levels; each provider is marked tested/previewed/untested
  (`crates.md` section 5.3 is the source). **Verification:** doc-consistency test extended to the
  provider table.

---

## P2: model and provider independence

### P2-01 Provider boundary and explicit execution inputs
* **Status:** implemented. **Evidence:** `fx-core::ModelProvider`; provider chosen from environment
  variables; the model supplies one validated decision and nothing else (`invocation_boundary.rs`).

### P2-02 Token, latency, cost and outcome by provider and attempt
* **Status:** partial. **Evidence:** per-work tokens and model/execution latency in the result
  `measurement`; no cost, no provider dimension in service metrics, no per-attempt aggregation.
* **Acceptance:** a result and metrics schema with provider, model, tokens, latency and (when the
  provider reports it) cost, per attempt. **Verification:** schema test; harness reports it.

### P2-03 Policy-driven escalation that does not assume a stronger model succeeds
* **Status:** missing in the product (see P1-07). **Acceptance:** the stop rule is a tested policy
  (budget, repeated failure), and a stronger tier's failure is a normal outcome.

### P2-04 FX in the production path
* **Status:** the request was to document FX as unavailable until implemented; **validated against
  the repository, that is not accurate for Rust FX.** Rust FX (`fx-core`, `fx-provider-http`) is
  implemented and is the model boundary of the production path (`crates.md` section 2): provider
  trait, three wire adapters, mock-server tests, live PAX/Compute-backed runs reported in
  `AGENTS.md` but not reproduced here. What is **not** implemented: the "FX reasons over observations
  / Decision Frontier" described in `project-observation-boundary.md` section 8 ("not implemented or
  specified anywhere in this repository today"), and the separate npm/Zig FX is not used and must
  not be assumed.
* **Acceptance:** README and `crates.md` keep Rust FX, the Decision Frontier and the npm/Zig FX
  distinct (they do). **Verification:** doc review.

### P2-05 Evaluate local and hosted providers on the same tasks and criteria
* **Status:** missing (G10). **Evidence:** no recorded real-model run of `chip work`/`serve`/`verify`;
  the opt-in tests (`CHIP_TEST_REAL_MODEL=1`) are not run in CI; Ollama and vLLM unrecorded.
* **Acceptance:** one task set with independent acceptance (P0-01) run against each provider; results
  stored as scripted vs real, separately labelled.
* **Verification:** the stored run, reproducible by command.

---

## P2: developer experience and release quality

### P2-D1 Predictable, documented CLI and service lifecycle
* **Status:** partial. **Evidence:** README documents `work`, `serve`, `verify`, flags and routes.
  The shipped binary also carries experiment and proof subcommands (D-01). **Acceptance:** the
  experiment subcommands become opt-in features, which is a breaking CLI change needing approval.

### P2-D2 Reproducible end-to-end coding tasks with independent acceptance
* **Status:** partial. **Evidence:** the courier fixture and `coding_agent` harness: reproducible,
  scripted judgment, real reality. **Acceptance:** more than one fixture, each with acceptance checks
  that do not run through the model.

### P2-D3 Scripted workflow tests are distinguishable from real-model evaluations
* **Status:** partial. **Evidence:** `coding-agent-evaluation.md` section 1 and `capabilities.md`
  label scripted vs real; no machine-readable marker. **Acceptance:** every report carries
  `judgment: scripted|real` in its schema.

### P2-D4 Regressions across seeded defects, incomplete implementations, repair attempts and ambiguity
* **Status:** partial. **Evidence:** one fixture with seeded defect, feature, repair and decision
  rungs; no history. **Acceptance:** a stored baseline per scenario and a comparison on each run.

### P2-D5 Release gates for correctness, compatibility, security and sustained execution
* **Status:** partial. **Evidence:** `release.yml` runs fmt, the wasm build, the workspace tests, the
  dependency audit, packaging and the smoke test on one target. **Missing:** a real-model gate, a
  compatibility check of public interfaces (adding `Capacity::max_retained` broke struct-literal
  construction in the baseline bench and would break embedders; found only by
  `cargo check --workspace --all-targets`), a security review step, a soak gate (P1-O2).
* **Acceptance:** `cargo check --workspace --all-targets` and the CI-run bench compile in the gate; an
  API-compat note for `serve_in` embedders per release.

---

## P2: capability completeness (evidence-gated, from `capabilities.md`)

None is justified until a real-model run shows the need (`capabilities.md` section 15).

* **P2-C1 Partial-edit mutation (G4):** only whole-file replace of at most 32 KiB exists. *Status:*
  missing. *Gate:* real runs show content loss or large-file changes. *Acceptance:* edit capability
  with read-back verification; files over 32 KiB changeable.
* **P2-C2 Create directory / delete / rename (G3):** missing; destructive, needs a recoverability
  design first (for example only inside a clean Git tree).
* **P2-C3 Lint, format and type-check verification (G7):** PAX owns the operations; Chip consumes
  them. *Status:* missing; no failing scenario yet.
* **P2-C4 Path-scoped, untruncated, untracked-aware Git diff (G5):** missing; composes around it today.
* **P2-C5 Independently owned predicate for `inspect` answers (G1b):** missing; until then an answer is
  grounded, never verified.

---

## P3: deferred (no current requirement; do not promote without a requirement, a contract and measurable acceptance)

* **P3-01 Durable session storage.** Assessed; **no-go** until resume-after-restart is a requirement
  (`session-store-comparison.md`). If it becomes one, SQLite is the measured leading candidate
  (proposed, not validated beyond the experiment), with an explicit durability contract and
  adapter-level integrity checks; power-loss durability untested.
* **P3-02 Process-restart recovery** (depends on P3-01 and P1-02).
* **P3-03 Advanced compaction.** In-memory retention-class compaction cut the stress heap from about
  82 MiB to 9 MiB in the experiment (`session-memory-experiment.md`); not in the product.
* **P3-04 Distributed work coordination and multi-process session ownership.**
* **P3-05 Higher-authority capabilities** (shell, network, secrets, Git mutation): excluded by
  `capabilities.md` section 14; each needs its own design and owner.
* **P3-06 Removal or opt-in-ing of frozen experiments** (`chip-wasm-reasoner` vs the Wasm decision
  pair, `chip-local-ml` vs `chip-laya-reasoner`): see D-02.
