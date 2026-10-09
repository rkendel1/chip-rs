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
| P0 correctness and safe execution | P0-02, P0-05, P0-07 | P0-01, P0-03, P0-04, P0-06 | P0-08, P0-09 (new), P1-V2 (raised to P0 by the audit) |
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
* **Audit 2026-10-09:** HA-02 reproduced the gap black-box: a requested feature never written, with the visible tests green, ends `completed`, `verified: true`, exit 0 while the independent acceptance check fails (case `v6`). No status change (still partial), evidence upgraded from "observed in the harness" to "reproduced on the real binary". See [`hostile-autonomous-agent-audit.md`](hostile-autonomous-agent-audit.md).
* **Design (2026-10-09):** acceptance becomes an explicit, runtime-owned predicate in the Work Contract: `acceptance.state`, `basis`, and `clarification_required` when none can be established (WC-06 to WC-09, WC-18); a completion on a baseline-only basis is reported as not goal-level. Implemented by **RIC-02**. Status unchanged (partial).

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
* **Audit 2026-10-09:** status unchanged (partial); evidence added. HA-05: the model timeout is a hard-coded 30 s with no configuration and no retry (a 429, 500, timeout or refusal ends the work at once, exit 3); HA-06: an empty, prose or JSON-plus-prose reply ends the work `failed`, exit 4, after one call; HA-11: a decision returned after cancel still executes and a cancelled work ends `failed`, not `cancelled`, and cancelling during `pax.test` waits for it (20.5 s measured). A real PAX hang ends at 300 s but orphans `cargo test` (HA-12). The acceptance criteria above stand; add "transient provider failure is retried within a stated budget" and "a cancelled work ends `cancelled`".

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
* **Audit 2026-10-09:** status stays partial; the audit **raises the urgency**. HA-01: verifying a repository executes its own `build.rs` and `.cargo/config.toml` rustc wrapper with the user's permissions, with no model write at all (`--kind verify`); HA-13: `config/secrets.toml`, `.aws/credentials` and `id_rsa` are readable and sent to the provider; HA-14: the reserved-name check is case-sensitive (`.ENV`, `.Git` accepted); HA-15: file content reaches the model verbatim and a `build.rs` write is allowed (chains to HA-01). The scripted model's attempts at writing outside the root, absolute paths, `.env`, `.git` and `shell.exec` were all refused.
* **Design (2026-10-09):** trust and tooling rules become contract fields (`authority.trust`, WC-13) and the test-surface policy becomes a verification requirement (WC-17). Implemented first, by **RIC-01**. Status unchanged (partial).

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
* **Audit 2026-10-09:** HA-12: after SIGKILL of `chip work`, `pax`, `cargo test` and the test binary keep running; after Chip's own 300 s timeout `cargo test` and the test binary survive. HA-16: after a kill the tree keeps the partial edit and a second run's first request carries nothing about the earlier attempt. HA-18: after a `serve` restart every id is 404 `work_not_found`. No temporary file was left by a killed write.

### P0-09 Writes are conditional on the state the model observed (lost-update protection)
* **Status:** missing (added by audit findings HA-03 and HA-04; a genuinely distinct gap: no existing item covers it).
* **Evidence:** a human edit made after Chip read a file is silently overwritten by the model's whole-file write, and the run still ends `verified` (HA-03, `c1`); two `chip work` processes on one directory are not excluded, and one reports `verified: true` for a write the other then replaced (HA-04, `c2`). `Environments` enforces one owner per mutable environment only within a process.
* **Prevents:** loss of a person's changes, and a `verified` result that no longer describes the tree.
* **Acceptance:** `project.write` carries the content hash of the version the model read (or the work holds a tree-level lock); a mismatch is a failed observation the model sees; a second process on the same directory is refused or serialized; the `c1` and `c2` scenarios end with the external edit intact.
* **Depends on:** none. **Constraint:** no new capability class; a precondition on the existing write.
* **Verification:** the audit scenarios `c1` and `c2` promoted to tests.
* **Design (2026-10-09):** the write precondition and the four fingerprint check points are specified as EL-10 and EL-11 and are part of **RIC-01**.

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
* **Design (2026-10-09):** attempt identity, strategy fingerprints and `attempt.failure` records are specified (EL-13); the contract version binds every attempt (WC-10). Implemented by **RIC-02** and **RIC-03**. Status unchanged (partial).

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
* **Audit 2026-10-09:** HA-16 confirms there is no resume contract and a second run starts blind; the roadmap decision (no persistence, P3-01 no-go) is unchanged.

### P1-03 A new attempt understands what was tried, failed, succeeded, and why
* **Status:** partial.
* **Evidence:** the product escalates with a deduplicated context (`DeduplicatedEscalationContext`);
  the evaluation harness builds a richer, assessed handoff (`packet.rs`) that is **test-only**; the
  decision protocol has no plan or hypothesis field (limit 6); a handoff does not fit the 2,000-byte
  goal (limit 4); file contents do not cross attempts (limit 5).
* **Acceptance:** the handoff structure is product code with a size bound that fits the surface;
  claims are labelled as assertions, facts as observations.
* **Verification:** the handoff negative tests (`coding_agent`) run against the product type.
* **Design (2026-10-09):** the handoff is a deterministic projection of contract and ledger (EL-17, BC-14), not transcript text. **RIC-03**, **RIC-04**.

### P1-04 Repair budgets; no endless retry or escalation loops
* **Status:** missing in the product (present in the evaluation harness only).
* **Evidence:** `chip work` has turn and execution limits and no repair budget or repeat limit
  (limit 3; `policy.rs` in the harness).
* **Prevents:** unbounded model spend on the same failure.
* **Acceptance:** repair budget and repeat-failure limit as a `LocalWorkPolicy` in the product, tested
  to stop on a repeated identical failure; budget reported in the result.
* **Verification:** scripted repeat-failure scenario; limits visible in JSON.
* **Audit 2026-10-09:** HA-09: the product does not detect repeated identical failures; 8 executions and 9 model calls are spent before `limit_reached` (the harness policy stops at the third). HA-06 suggests the budget should also cover malformed replies.
* **Design (2026-10-09):** repeat detection and the repair budget are classifier rules BC-R5 and the typed strategy catalog (BC-09). **RIC-04**.

### P1-05 Bounded context construction and evidence selection
* **Status:** partial.
* **Evidence:** `CHIP_CONTEXT_BUDGET_BYTES` / `--context-budget-bytes` and a `ContextReport` with
  omissions (`software_work.rs`; `chip-core/tests/context_discipline.rs`); product capabilities are
  evidence-reuse-prohibited, so context is rebuilt from fresh observations. Unmeasured: how a real
  model copes with truncation.
* **Acceptance:** documented selection rules; a test that the budget is never exceeded and that an
  omitted observation is disclosed to the model.
* **Verification:** context-discipline tests plus a real-model run (P2-05).
* **Audit 2026-10-09:** HA-07: with no budget, request size grows by about one observation per call (4.9 KB to 274 KB over nine calls reading 32 KB each); the Ollama adapter never sends `num_ctx`. HA-08: test output beyond 256 KiB is cut at the head with no truncation notice, so a decisive failure at the end never reaches the model.
* **Design (2026-10-09):** context is a deterministic function of (contract, ledger, budget) with disclosed omissions (EL-17); the micro-step gate narrows the next call (MS). **RIC-03**, **RIC-05**, **RIC-06**.

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
* **Audit 2026-10-09:** HA-10 confirmed by execution: one provider per process; `escalate` is terminal with only a reason; the ladder is not linked into `chip work`.
* **Design (2026-10-09):** escalation is classified, not assumed: only `reasoning_insufficiency` (BC-R7) may trigger a stronger-model handoff, never missing criteria, authority or trustworthy evidence (BC-14, BC-15); the human-addressed stops are specified (blockage-classifier section 8). **RIC-04** (classification), **RIC-07** (routing).

### P1-08 Each escalation carries evidence, prior attempts, constraints and remaining uncertainty
* **Status:** partial.
* **Evidence:** as P1-03; the harness packet assesses completeness and refuses a handoff that presents
  a claim as fact.
* **Acceptance / verification:** as P1-03, plus a test that an escalation lacking any of the four is
  refused.
* **Audit 2026-10-09:** HA-10: the escalated result a client receives has the model's reason but no structured handoff.
* **Design (2026-10-09):** the escalation payload is built only from the contract and ledger (BC-14); claims are labelled as claims (EL-02). **RIC-04**.

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
* **Audit 2026-10-09:** **priority raised from P1 to P0 by HA-02.** Five of five constructed false-success cases (assertions weakened, tests ignored, failing tests deleted, requested feature absent, visible test special-cased) end `verified: true`, exit 0 on the real binary; the same cases emptying every test or disabling the test target are correctly refused (`not_run`, `no-tests-executed`). The result lists `paths_written` including the edited test file but raises no signal. The ID is kept so existing references remain valid.
* **Design (2026-10-09):** test-surface protection is a verification requirement (WC-17) and an eligibility column in the ledger matrix (EL-14, evidence-ledger section 7). **RIC-01** first, then **RIC-03**.

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
* **Design (2026-10-09):** every completion decision cites ledger records, verified for integrity before the decision (EL-14, EL-15). **RIC-03**.

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
* **Design (2026-10-09):** evidence is bound to attempt, execution, contract version and the repository fingerprint at production (evidence-ledger section 1). **RIC-03**.

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
* **Audit 2026-10-09:** HA-17 measured: with `--max-retained-work 10`, 300 escalated works left `retained_work` 300 and `evicted_work` 0 (about 30 KB of server RSS each).

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
* **Audit 2026-10-09:** HA-21: the README never says where to obtain PAX (the URL appears only in CI workflow files); a clean checkout builds and passes 1139 tests with PAX on `PATH` once it is found.

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
* **Design (2026-10-09):** the stop rule is the classifier: a stronger tier's failure is a normal outcome and a second `reasoning_insufficiency` at the top tier is a human-addressed stop (BC-15). **RIC-07b**.

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
* **Audit 2026-10-09:** the audit built the harness for this (fixture with two seeded defects and hidden acceptance tests, `audit/hostile/`) and could not run it: no weights are reachable from the audit environment, no GPU (`audit-evidence/hostile-audit/local-model-probe.txt`). HA-19: the repository's opt-in real-model tests print `SKIPPED` and report `ok`.
* **Design (2026-10-09):** the evaluation is staged in **RIC-07**: 7a baseline of today's behavior on a fixed suite (no dependency on the new contracts), 7b routing, 7c comparative runs. The 85 % local-model figure is an **evaluation goal**, not an implementation criterion, until 7a has a reproducible real-model baseline.

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
* **Audit 2026-10-09:** HA-20: the all-public `Capacity` struct broke `cargo check --workspace --all-targets` when a field was added, while plain `cargo check --workspace` passed; HA-19: skipped real-model tests count as passes.

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

## Runtime intelligence contracts: implementation tickets (RIC)

Design: [`work-contract.md`](work-contract.md), [`evidence-ledger.md`](evidence-ledger.md),
[`blockage-classifier.md`](blockage-classifier.md), [`micro-step-proposal-gate.md`](micro-step-proposal-gate.md).
These tickets *implement and refine* existing items; they do not replace them. Each ticket lists the existing
roadmap ids it refines, the normative requirements (`WC-`, `EL-`, `BC-`, `MS-`) that are its acceptance criteria,
its dependencies and its verification. Nothing here is implemented; every status is **not started**.

**Principle (repeated because it is the failure mode to avoid):** a stronger model is not a substitute for missing
acceptance criteria, authority, or trustworthy evidence. Only a classified `reasoning_insufficiency` (BC-R7) may
trigger a stronger-model handoff.

**Order and why.** Trust and verification first (RIC-01), because a contract or a ledger on top of a verifier that
can be gamed (HA-01, HA-02) builds on sand. The Work Contract second (RIC-02): it becomes the runtime's stable control
object, and the ledger and classifier read it instead of inventing state of their own. Dependency note: the
contract's acceptance derivation (WC-06, rows 2 to 4 of its table) needs a **baseline verification**, so that single
pre-model step belongs to RIC-02; RIC-06 covers the rest of deterministic pre-model evaluation (candidate derivation,
runtime-scheduled re-verification, cost reporting). A real-model **baseline of today's free-form behavior** (RIC-07a)
has no dependency on RIC-01 to RIC-06 and should be taken as soon as a model is available; it is what the later
improvements are measured against.

| Ticket | Priority | Status | Depends on | Refines | Closes audit findings |
| --- | --- | --- | --- | --- | --- |
| RIC-01 Trustworthy execution and verification | P0 | not started | none | P0-06, P0-09, P1-V2, P0-04 (partly) | HA-01, HA-02 (v3a to v3c), HA-03, HA-04, HA-12, HA-14 |
| RIC-02 Work Contract: schema, immutability, versioning, clarification, baseline | P0 | not started | RIC-01 | P0-01, P1-01, P1-O4 | HA-02 (v6, v7: labelled, closed only with submitter criteria), HA-05 (budgets/timeouts as contract fields) |
| RIC-03 Evidence Ledger: invalidation and attempt linkage | P1 | not started | RIC-02 | P1-V4, P1-V6, P1-03, P1-05, P1-02 (in-process) | HA-08, HA-16 (in process) |
| RIC-04 Deterministic blockage classification and typed repair strategies | P1 | not started | RIC-03 | P1-04, P1-06, P1-07, P1-08, P2-03 | HA-06, HA-09, HA-10 (classification half) |
| RIC-05 Bounded micro-step proposals (default off) | P2 | not started | RIC-02, RIC-03, RIC-04 | P1-05 | HA-06, HA-07 (mitigation) |
| RIC-06 Deterministic pre-model evaluation (beyond the baseline) | P1 | not started | RIC-02, RIC-03 | P1-05, P1-V3 | HA-07 (mitigation) |
| RIC-07 Local-first routing and real-model evaluation | P1 (7a) / P2 (routing) | blocked on a model (7a) | 7a: none; routing: RIC-01 to RIC-06 | P2-03, P2-05, P2-D3, P2-D4 | HA-10, HA-19 |
| RIC-08 Shadow-mode micro-model evaluation | P2 | implemented, shadow only; real-model evaluation blocked on a model | none (interim stand-ins for RIC-02, RIC-03) | P2-03 (measurement half) | none |

### RIC-01 Trustworthy execution and verification (P0)
* **Scope.** The minimum that removes the two Critical audit findings, in a form that later *becomes* contract and ledger
  data: (1) a **trust gate** for project tooling: no `pax.test` in a repository not marked trusted unless the environment
  isolates it or the submitter makes an explicit trust decision (WC-13; HA-01); a `warn` rollout first, then `enforce`;
  (2) **test-surface protection**: record the baseline test surface (PAX `project.observe` test facts when available,
  else a path-based fingerprint of test-bearing files) and refuse `verified` when it changes without an authorizing
  decision (WC-17, EL-14 test-surface column; HA-02); (3) **write preconditions** and a tree-fingerprint check at the
  four points of EL-10 (EL-01, EL-10, EL-11; HA-03, HA-04); (4) children are killed with their parent or process group
  (HA-12); (5) the reserved-name check folds case (HA-14).
* **Acceptance.** EL-01, EL-10, EL-11, EL-18, EL-19, WC-13 (trust), WC-17 (surface), and the audit scenarios `o4-*`, `v3a`, `v3b`,
  `v3c`, `c1`, `c2`, `i1`, `o5-write-dot-ENV` promoted from `audit/hostile` to CI tests, each ending as the requirement
  states; `rollout=warn` and `rollout=enforce` both tested.
* **Dependencies / constraints.** None. No new capability class; no persistence; the isolation *mechanism* belongs to an
  `EnvironmentProvider` outside this repository, so the ticket only enforces the gate and documents the requirement.
* **Verification.** The promoted scenarios; a differential test that the gate adds no permission (MS-05 style) for the
  trust decision; the existing 1139-test workspace run unchanged.

### RIC-02 Work Contract (P0)
* **Scope.** `WorkContract` in `chip-core` with canonical serialization, immutability, versions and history (WC-01 to
  WC-05, WC-19, WC-20); acceptance derivation including the baseline verification and `clarification_required`
  (WC-06 to WC-09, section 4 of the design) in `warn` then `enforce`; version binding on events (WC-10, WC-11); authority
  and budgets including configurable model timeout and re-ask budget (WC-12, WC-14, WC-15); `compile_to_spec`; result
  fields (`acceptance_basis`, `verified_against`, WC-18).
* **Acceptance.** WC-01 to WC-20 as written; audit scenarios `v6` and `v7` are *labelled* `verified_against: failing_baseline_tests,
  goal_level: false` without submitter criteria, and are not verified until a submitter criterion that exercises the feature
  or the general behavior passes (WC-18); a green-baseline `change` goal with no criteria ends `escalated`
  (to a human) with `clarification_required`; `e-*` limits reproduce through contract
  budgets; HA-05's slow-server scenario passes with a larger configured timeout.
* **Dependencies / constraints.** RIC-01. Public API additive; struct-literal breakage avoided (`#[non_exhaustive]` or
  builders, HA-20). No FX change.
* **Verification.** The table-driven tests named in each requirement; the existing event-order and audit tests unchanged.

### RIC-03 Evidence Ledger (P1)
* **Scope.** In-memory ledger with record schema, hash chain, dependency DAG, `freshness()` and `verify_ledger()`
  (EL-02, EL-03, EL-05, EL-08, EL-09, EL-12 to EL-17); `attempt.failure` records and attempt identity (EL-13; P1-01);
  completion eligibility from the ledger (EL-14); deterministic context projection replacing transcript carry-over
  (EL-17; P1-05); disclosure of truncated diagnostics (HA-08).
* **Acceptance.** EL-01 to EL-19, including the eligibility matrix of the design, section 7, as one table-driven test.
* **Dependencies / constraints.** RIC-02 (every record binds a contract version). The existing `EvidenceStore` remains for
  reuse only. No persistence.
* **Verification.** Property tests for invalidation propagation; tamper tests for integrity; the audit scenarios `v5`,
  `c1`, `c2`, `i2` (second run in-process context) and the `o7-flood-*` cases.

### RIC-04 Deterministic blockage classification and typed repair strategies (P1)
* **Scope.** `classify()` with the eight rules and signals of the design (BC-01 to BC-13); a **typed strategy catalog**
  (a strategy has an id, a type from a closed set, a target criterion, target files, and a fingerprint; initial types:
  `read_more_context`, `run_single_test`, `narrow_edit`, `revert_and_retry`, `change_target_file`, `micro_step`);
  repair budget and repeat-failure stop (P1-04); terminal mapping reusing existing states (section 8 of the design);
  the human-addressed payloads (P1-08).
* **Acceptance.** BC-01 to BC-13 and the corpus of the design, section 6, run in CI; `e-repeat-identical-wrong-write-then-
  test` ends after `repeat_threshold`, not at the execution limit (HA-09).
* **Dependencies / constraints.** RIC-03. No model call in the classifier; no stronger-model handoff yet (RIC-07).
* **Verification.** The corpus; a source-level test forbidding clock/model/random use in the module (BC-01).

### RIC-05 Bounded micro-step proposals (P2, default off)
* **Scope.** `chip.micro-step.v1` parser, validation V1 to V9, mode selection and events, budgets and loop guards
  (MS-01 to MS-11).
* **Acceptance.** MS-01 to MS-11; the differential no-bypass test (MS-05) passes with every audit attack; the setting is
  off by default (MS-02).
* **Dependencies / constraints.** RIC-02, RIC-03, RIC-04. It MUST NOT become the default or be described as an improvement
  before MS-12.
* **Verification.** The tests above; the A/B evaluation belongs to RIC-07.

### RIC-06 Deterministic pre-model evaluation (P1)
* **Scope.** Candidate derivation from failing-test references and PAX structure facts (WC-16); runtime-scheduled
  re-verification after invalidating writes (BC-08); bounded cost and its reporting (WC-21 to WC-23); skipping when it
  cannot change the contract.
* **Acceptance.** WC-16, WC-21, WC-22, WC-23, BC-08.
* **Dependencies / constraints.** RIC-02, RIC-03. The performance invariant (`crates.md` section 1): no unnecessary
  subprocesses; the cost of pre-model steps is reported and bounded.
* **Verification.** Determinism test (byte-identical candidates); cost report on a `verify` run (exactly one verification).

### RIC-07 Local-first routing and real-model evaluation (P1 for 7a, P2 for routing)
* **7a. Baseline evaluation of the current system (no dependencies; blocked on a model).** The fixed task suite (the
  `audit/hostile` fixture plus further fixtures with independent acceptance), a pre-registered evaluation plan (suite,
  repetitions *N*, decision rules, reported variance), run against at least one real local model with today's free-form
  behavior. Scripted and real results are never mixed. **This baseline is what every later change is measured against.**
* **7b. Routing.** A configurable stronger tier, handoff only on BC-R7 with budget (BC-14, BC-15, BC-16; P2-03), handoff
  outcomes recorded; human-addressed stops otherwise.
* **7c. Comparative evaluation.** Free-form vs micro-step (MS-12), local-only vs escalation-enabled, with cost,
  tokens, time, escalation outcomes (P2-02, P2-05).
* **Acceptance.** 7a: the plan, the stored traces and the reported baseline success and regression rates with variance.
  7b: BC-14 to BC-16. 7c: results published before any default changes.
* **Evaluation goal, not an acceptance criterion.** The 85 % local-model verified-completion target is measured on the
  fixed suite once a reproducible real-local-model baseline exists. No ticket may cite it as an implementation criterion
  before then (WC §11, MS-13).
* **Dependencies / constraints.** 7a none; 7b RIC-01 to RIC-06; 7c RIC-05 and 7b. Provider integration stays with FX.
* **Verification.** The stored evaluation artifacts.

### RIC-08 Shadow-mode micro-model evaluation (P2; implemented as observation only)
* **Scope.** An optional, explicitly configured, provider-neutral shadow interface (`chip work --micro-shadow`,
  `CHIP_MICRO_*`) through which a small model classifies the last failure and nominates one repair strategy from a
  closed catalog, under the versioned `chip.micro.v1` contract with strict validation. The nomination is recorded
  beside the runtime's deterministic outcome and nowhere else. A fixed, labelled evaluation fixture and a
  reproducible harness (`cargo bench -p chip-cli --bench micro_eval`) measure it. Specified in
  [`micro-model-shadow.md`](micro-model-shadow.md); the controlled-ablation follow-up (expanded executed fixture, frozen
  held-out set, candidate gating, A/B harness; configuration C not enabled; real-model runs blocked) is in
  [`micro-model-ablation.md`](micro-model-ablation.md).
* **Acceptance.** The checklist of `micro-model-shadow.md` section 9. It adds no authority (MS-02, MS-05 posture:
  off by default, nothing accepted that the normal path would not accept: here nothing is accepted at all).
  Contract version and snapshot identity are interim stand-ins until RIC-02 and RIC-03.
* **Dependencies / constraints.** None to build. No execution path, no new dependency, no persistence, no change to
  the Work Contract or Evidence Ledger. It cannot show reduced larger-model calls or better verified completion:
  that needs the controlled ablation of RIC-07 (7c). Any authority change is a separate, reviewed ticket gated by
  held-out evidence (`micro-model-shadow.md` section 10).
* **Verification.** Validator positive and negative tests, shadow-isolation tests through the real binary, the
  fixture's structural checks, the scripted self-test and replay. The real-model evaluation is **blocked** (no model
  available in the build environment) and says so.

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
