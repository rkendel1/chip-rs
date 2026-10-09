# Differential Evidence Ledger (`chip.evidence-record.v1`)

**Status: normative design. Not implemented.** See [`work-contract.md`](work-contract.md) for the
set of four contracts and the principle they serve. This one answers: *what do we know about the
repository and the previous attempts, how do we know it, and is it still true?*

> A test result is never current merely because it passed at some earlier point in the work.
> When freshness cannot be established, the result is **inconclusive**, never implicitly valid.

Words MUST/SHOULD/MAY as in RFC 2119. Requirements are `EL-nn`, each with an **Acceptance** condition a
test can decide.

## 0. Scope and relation to what exists

Today (`chip-core`): observations are recorded per work; an in-memory `EvidenceStore` (keyed by
capability plus inputs, with an optional caller-supplied `StateToken`) lets an identical operation avoid
re-execution; `evidence_reuse_prohibited` forbids reuse for the product capabilities; completion
predicates (`ObservationPredicate::satisfied_by_trajectory`) judge freshness **by position in the
trajectory** ("a pass counts only if no content-changing write follows it"); escalation context is
rebuilt from observations and a `Prior decisions` / `Ruled out` rendering.

The ledger **generalizes those three mechanisms into one authoritative structure**:

| Today | In this design |
| --- | --- |
| freshness by trajectory position | freshness by *dependency and repository-state binding* (EL-04 to EL-12) |
| `EvidenceStore` keyed by invocation | stays as a reuse optimisation for capabilities that permit reuse; **it is not a source for completion or context** |
| `StateToken` (opaque, caller-owned) | the repository state reference of a record is a `RepoStateRef` (section 2); `StateToken` remains the type for capability-owned state |
| rendering "prior decisions / ruled out" from the loop | a deterministic projection of the ledger (EL-17) |

The ledger is in memory, per work, owned by Chip. **No event-sourcing platform, no persistence, no
cross-work or global memory.** It is exported in the result and events for diagnosis. FX and PAX are
unchanged; PAX remains the only interpreter of test results.

## 1. What a record is

```jsonc
{
  "schema": "chip.evidence-record.v1",
  "id": "ev-17",                      // monotonic within the work; the order of ids is the authoritative order
  "seq": 17,                          // same as the id number; wall time below is informational only
  "type": "observation.read | observation.list | observation.search | observation.git | observation.write |
           observation.structure | verification.pax_test | verification.pax_observe |
           attempt.failure | repo.external_change | claim.model",
  "content_hash": "sha256:…",        // of the canonical observation payload
  "chain_hash": "sha256:…",          // sha256(previous.chain_hash || canonical record without chain_hash)
  "produced_by": { "attempt_id": "att-2", "execution_id": "work_…-exec-6", "capability": "project.read" },   // execution_id null for non-executions
  "contract": { "version": 3, "criteria_hash": { "AC1": "sha256:…" } },
  "depends_on": ["ev-12", "ev-14"],   // or "unknown" (section 5)
  "scope": { "paths": ["src/money.rs"], "kind": "path_set | tree | none" },
  "repo_state": { "before": "sha256:…", "after": "sha256:…", "taken_at_seq": 17 },       // fingerprints, section 2
  "binding": { "source_hash": "sha256:…", "test_surface_hash": "sha256:…" },              // verification.* only
  "payload_ref": "obs-17",            // the existing observation; the ledger stores hashes and bounded excerpts, not payload copies
  "produced_at": "2026-10-09T12:00:00Z",
  "invalidation": null,               // or { "cause": "write|external_change|dependency|contract_change|integrity_failure", "by": "ev-21", "at_seq": 21 }
  "claim": false                      // true only for type claim.model
}
```

`eligible_for_completion` is **derived**, never stored: it is the result of `freshness(record, now)` (EL-08)
combined with the contract rules of EL-14. A model can never set or read-write any field of a record.

`claim.model` records hold things the model *said* (a summary, a reason). They are included in handoffs
**labeled as claims**, are never dependencies of an eligible record, and are never eligible (EL-02).

## 2. Repository state references

`RepoStateRef` is a **fingerprint**: SHA-256 over the sorted list of `(relative path, sha256 of content)` for
the files in scope, using Chip's own deterministic traversal (project-root rules of `chip-project`, so reserved
names and symlinks are excluded exactly as for capabilities; `target/`-style build directories are excluded by
a fixed, documented list). Two scopes exist:

* **path_set** fingerprint: the listed paths only (cheap; used for per-file observation freshness);
* **tree** fingerprint: all in-scope files (used to bind verification and to detect external change).

A fingerprint that cannot be computed completely (a size or count bound is exceeded, a file is unreadable) is
marked `incomplete: true` and **can only support `inconclusive`**, never `fresh`.

* **EL-01.** `RepoStateRef` computation MUST be deterministic and MUST depend only on file bytes and
  relative paths, not on mtimes, inode data or iteration order.
  *Acceptance:* two checkouts with identical content in different directory orders and different mtimes produce
  identical fingerprints; touching a file without changing it does not change the fingerprint; changing one byte does.

### 2.1 Test surface

The *test surface* is the part of the repository that decides whether verification means anything: the tests
themselves. The contract's `test_surface` policy (WC-17) is decided against a **test-surface hash** recorded at baseline
and at every verification.

* **EL-18.** The test surface MUST be computed deterministically at two levels, using the most precise one available,
  and compared with the baseline:
  *Level 2 (preferred):* per-test facts from PAX (`project.observe` test declarations: identity, file, and the hash of
  the test's source span or, when PAX gives none, of its file) so that a removed, `#[ignore]`d, renamed or modified
  existing test is identified individually and a purely added test is allowed.
  *Level 1 (always available):* the set of **test-bearing files**, found by the ecosystem's conventional locations and
  markers (for Rust: files under `tests/`, files named `*_test.rs`/`tests.rs`, and files containing a test attribute
  found by Chip's own deterministic search) with their content hashes; here a removed or modified existing test-bearing
  file counts as a change, a new test file is allowed, and the cost of this precision loss (adding a test to an
  existing test file needs an authorization) is accepted for safety.
  If neither level can be computed for the ecosystem, the surface is `unknown`, and under `protect_existing` that is
  `Inconclusive (scope_unknown)` for completion, never silently fine.
  *Acceptance:* the audit fixtures `v3a` (assertions weakened), `v3b` (`#[ignore]` added) and `v3c` (failing tests
  deleted) are each reported as a test-surface change at Level 1 and Level 2; adding a new test file is not a change; an
  ecosystem with no computable surface yields `Inconclusive`; Level 2 identifies which test changed.
* **EL-19.** Fingerprint computation MUST be bounded and its cost reported: a stated maximum number of files and bytes
  per tree fingerprint, beyond which the fingerprint is `incomplete` (EL-01 consequence: only `Inconclusive`), and
  every fingerprint taken is counted in the result with its duration. Optimizations (incremental hashing from tracked
  writes, caching) MUST NOT change the fingerprint value or weaken change detection.
  *Acceptance:* a repository over the bound yields `incomplete` and an inconclusive verification; a cached run and an
  uncached run produce identical fingerprints and identical detections on the external-edit fixtures; the result lists
  the number and cost of fingerprints.

## 3. Invariants

* **EL-02.** The ledger is the only source of evidence about prior attempts. Free-form conversation history
  MUST NOT be re-injected into a model request as an alternative source of truth. Model statements enter only
  as `claim.model` records.
  *Acceptance:* a request builder test shows every non-contract, non-goal statement in a request cites a ledger
  record id, and that removing a record from the ledger removes it from the request; a transcript-injection test
  (a prior model reply asserting "tests passed") produces no eligible evidence.
* **EL-03.** Every record MUST carry `content_hash`, `chain_hash`, `contract.version`, `produced_by` and
  `depends_on`; append MUST fail if any is missing, if `depends_on` references a record not yet in the ledger, or
  if the record would create a cycle.
  *Acceptance:* append-validation unit tests for each failure.

## 4. Invalidation rules (normative)

| Event | Required behavior | Req |
| --- | --- | --- |
| File write (`observation.write` of path *P*) | Every earlier record whose `scope.paths` contains *P*, or whose `scope.kind = tree`, is invalidated (`cause: write`); invalidation propagates to every record that transitively `depends_on` an invalidated record (section 5) | EL-04 |
| Derived observation (a record computed from other records, e.g. a candidate list, a structure summary, a diff) | Declares `depends_on` for every input record at production; a record that cannot enumerate inputs declares `unknown` (section 5) | EL-05 |
| Test execution (`verification.pax_test`) | At start and at end, Chip computes the **tree** fingerprint and the **test-surface** hash; `binding.source_hash` and `binding.test_surface_hash` are recorded only if start and end are equal; otherwise the record is `inconclusive` (`mid_run_change`) | EL-06 |
| Subsequent relevant write | A verification record is stale for completion as soon as any write touches a path in its scope. With no narrower scope from PAX, the scope is the whole tree minus excluded build directories | EL-07 |
| External repository change | Detected by a fingerprint mismatch at the checks of EL-10. Chip appends `repo.external_change` (paths changed, hashes before/after), invalidates affected records, and **protects the newer user content**: no write may overwrite a path whose current hash differs from the hash the model last observed (the write precondition of roadmap item P0-09) | EL-10, EL-11 |
| Failed attempt | Chip appends `attempt.failure`: `strategy_id`, `strategy_fingerprint`, outcome, the ids of the failure evidence, the criteria it targeted, and the budgets remaining at the time | EL-13 |
| Completion decision | Requires fresh `verification.*` evidence for every `verification.required` entry of the *current* contract version, with the tree fingerprint at decision time equal to the record's binding (EL-14) | EL-14 |
| Work continuation (a new attempt, a stronger-model handoff, a human reply) | Context is reconstructed from the ledger by the projection of EL-17, not from transcript text | EL-02, EL-17 |

### 4.1 Requirements for the rules

* **EL-04.** *Acceptance:* write *P*, then ask freshness of an earlier `observation.read` of *P*, an earlier
  `observation.list` of its directory, and an earlier `observation.git` diff: all `stale (write)`. An earlier read of
  an unrelated path stays `fresh`.
* **EL-05.** *Acceptance:* a derived record appended with an empty or missing `depends_on` and not `unknown` is
  rejected (EL-03); a derived record whose input is invalidated becomes `stale (dependency)` with the chain of ids.
* **EL-06.** *Acceptance:* a test run during which a file is changed (scripted external write between start and end)
  yields `inconclusive (mid_run_change)` and cannot support completion; a run with no change yields a record whose
  `binding` equals the fingerprints computed independently by the test.
* **EL-07.** *Acceptance:* pass, then write any in-scope path, then ask completion eligibility: `ineligible (stale)`;
  re-run: eligible; a write to a path outside the verification's declared scope leaves it eligible.
* **EL-08.** The freshness function is pure: `freshness(record, ledger, repo_now) -> Fresh | Stale(cause, by) |
  Inconclusive(reason)` with no model call, clock or randomness. `Inconclusive` reasons are a closed set:
  `incomplete_fingerprint`, `unknown_dependency`, `dangling_dependency`, `mid_run_change`, `integrity_unverified`,
  `scope_unknown`.
  *Acceptance:* a table-driven test covers each reason and asserts that none maps to `Fresh`.
* **EL-09.** `Inconclusive` MUST be treated as ineligible for completion and as a classifier input
  (`missing_evidence` or `verification_conflict`, see [`blockage-classifier.md`](blockage-classifier.md)).
  *Acceptance:* a completion attempt over an inconclusive record is refused with the reason code in the result.
* **EL-10.** Chip MUST compare the current fingerprint to the last recorded one at: (a) immediately before each
  write (path_set), (b) at the start and end of each verification (tree), (c) immediately before each completion
  decision (tree), and (d) before assembling a handoff (tree).
  *Acceptance:* an external edit scripted at each of the four points is detected at that point and recorded as
  `repo.external_change` with the changed paths; an edit outside the scope of a given check does not invalidate it.
* **EL-11.** A write MUST carry the content hash of the version the model last observed for that path (or
  `absent`); a mismatch is a failed observation shown to the model, not an overwrite.
  *Acceptance:* the audit scenario `c1` (human edit between read and write) ends with the human edit intact and a
  recorded conflict; two processes (`c2`) cannot both obtain a fresh verification for a tree one of them replaced.
  *(Implements roadmap P0-09; the ledger provides the record, the write path enforces the hash.)*
* **EL-12.** Unknown dependencies are handled conservatively: a record with `depends_on: unknown` is invalidated by any invalidation in its scope, and a missing dependency makes the dependent `Inconclusive`.
  *Acceptance:* the two tests of section 5 (unknown dependency after an unrelated in-scope write; dangling dependency).
* **EL-13.** An `attempt.failure` record MUST name the strategy and its fingerprint (a stable hash of the
  strategy's type and its target criterion and files, not of free text), the failure evidence ids, and the budgets
  left; it never contains the model's explanation as evidence (that is a separate `claim.model` record).
  *Acceptance:* two attempts with the same strategy fingerprint are detected as repeats by comparing the field,
  without reading prose.
* **EL-14.** Completion MUST require, for every required verification: a record of the right type that is `Fresh`,
  bound to the current `contract.version` (or to an earlier version with unchanged `criteria_hash` for what it
  supports), with `binding.source_hash` equal to the **current** tree fingerprint, and whose `test_surface_hash`
  satisfies the contract's `test_surface` policy against the baseline.
  *Acceptance:* the matrix of section 7 is a table-driven test: each cell's expected eligibility is asserted, and
  no cell with a stale, inconclusive, wrong-version or tampered-surface record is eligible.

## 5. Dependencies

* Dependencies are a DAG over record ids; an id may only refer to an earlier record (EL-03), so cycles cannot occur.
* **Propagation (EL-04, EL-12).** When record *R* is invalidated, every record that lists *R* in `depends_on` is
  invalidated with `cause: dependency, by: R`, recursively. Propagation is computed on demand by
  `freshness()` walking `depends_on` (bounded by the ledger size), not by mutating records.
* **`depends_on: unknown`.** A producer that cannot enumerate its inputs declares `unknown`. The record is then
  treated as depending on **every** earlier record in its scope: any invalidation in that scope invalidates it.
  If the scope itself is unknown (`scope.kind = none` with `unknown`), any write at all invalidates it.
  *Acceptance:* an `unknown`-dependency record is `stale` after an unrelated in-scope write and `Inconclusive
  (scope_unknown)` when it has no scope.
* **Dangling dependency.** A dependency id missing from the ledger (corruption, bug) makes the dependent
  `Inconclusive (dangling_dependency)`. *Acceptance:* a ledger with a removed record flags exactly its transitive
  dependents.

## 6. Integrity

* **EL-15.** `content_hash` MUST be verified when a record's payload is read back (`payload_ref`), and
  `chain_hash` links each record to its predecessor so that truncation, reordering or alteration of the ledger is
  detectable. `verify_ledger()` MUST run before every completion decision and before every handoff.
  *Acceptance:* flipping one byte of a stored payload, removing a middle record, or swapping two records is
  detected, the affected records are `Inconclusive (integrity_unverified)`, the completion is refused, and the
  classifier receives `verification_conflict`. *(The chain detects accidental damage and logic errors inside
  one process; it is not a defense against a malicious runtime or host.)*
* **EL-16.** The ledger stores hashes, ids, bounded excerpts (default 1 KiB) and references, **not** payload copies;
  payload storage remains the existing observation store and is bounded by the retention rules of roadmap
  P1-O1/P1-O2. A record needed by the current contract MUST NOT be evicted; others may be, leaving their hash and
  a `payload_evicted` flag (their eligibility is unaffected, their payload is simply not re-readable).
  *Acceptance:* a long run's ledger memory is bounded by a stated constant plus per-record excerpt limits;
  eviction never removes a record cited by a required verification.

## 7. Completion eligibility matrix (EL-14, testable)

| Verification record | Contract version | Tree now vs `binding.source_hash` | Test surface vs baseline | Eligible? |
| --- | --- | --- | --- | --- |
| passed, fresh | current | equal | unchanged | **yes** |
| passed, fresh | current | equal | changed, no authorizing delta | no (`verification_conflict`) |
| passed, fresh | current | equal | changed, `human_decision` delta | yes, authorization recorded |
| passed | current | differs (write or external change) | any | no (stale / external change) |
| passed | older, `criteria_hash` unchanged | equal | unchanged | yes |
| passed | older, `criteria_hash` changed | equal | unchanged | no (contract change) |
| passed, start≠end fingerprint | current | any | any | no (`inconclusive: mid_run_change`) |
| `not_run`, `failed`, `error`, `ambiguous`, `unsupported` | current | any | any | no |
| passed but payload hash mismatch / chain broken | any | any | any | no (`integrity_unverified`) |
| none | — | — | — | no (`missing_evidence`) |

## 8. Projection into context (EL-17)

* **EL-17.** The model-facing context is a **deterministic function of (contract version, ledger, budgets)**:
  outstanding criteria with their current evidence status; the freshest eligible observations relevant to the
  candidates; every `attempt.failure` record (strategy, outcome, ids), newest first, within the context budget;
  and, labeled as claims, relevant `claim.model` entries. Records omitted for the budget are named, never
  summarized, and an omitted record that is needed by an outstanding criterion makes the criterion's status
  `needs_evidence` rather than silently complete (this keeps the existing "omissions are disclosed" rule).
  *Acceptance:* identical (contract, ledger, budget) inputs yield byte-identical requests; a stale record never
  appears as current; a handoff built after a failed attempt contains the failure records and not the transcript.

## 9. Failure modes and what the ledger does about them

| Scenario | Ledger behavior |
| --- | --- |
| Model weakens a test and re-runs | verification binds a `test_surface_hash` different from baseline: not eligible unless authorized (EL-14) |
| Model edits after a green run and claims completion | green run stale (EL-07); runtime requires fresh verification |
| A person edits during the run | `repo.external_change`; affected evidence invalid; model's next write conflicts (EL-11) |
| Two processes on one tree | the second's verification binding differs from the tree at its decision (EL-10c): ineligible; exclusion itself belongs to P0-09 |
| Output cut by a cap | the observation records `truncated: true`; a verification whose diagnostics were truncated is still valid for pass/fail (PAX status) but marks `diagnostics_incomplete` for the classifier |
| Evidence evicted for memory | hash and flag remain; eligibility unchanged; re-reading requires a fresh observation |

## 10. Non-goals

Persistence or replay across restarts, global or cross-project memory, a query language, distributed
consistency, cryptographic non-repudiation, and replacing PAX's interpretation of test results. The ledger records
and orders; it does not decide whether tests are adequate.

## 11. Traceability to the audit

| Finding | Ledger requirements that close it |
| --- | --- |
| HA-02 (false success by editing the oracle) | EL-06, EL-07, EL-14, the test-surface column of section 7 |
| HA-03, HA-04 (lost updates) | EL-10, EL-11 |
| HA-08 (silent truncation) | section 9, `truncated`/`diagnostics_incomplete` |
| HA-16 (second run starts blind) | EL-02, EL-13, EL-17 (within one process; cross-restart stays P3-01) |
