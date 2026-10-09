# Work Contract (`chip.work-contract.v1`)

**Status: normative design. Not implemented.** Nothing in this document describes behavior that exists
today, and no claim about local-model performance follows from it. It is the first of four contracts
that together let Chip turn limited model intelligence into independently verified work:

| Contract | Question it answers | Document |
| --- | --- | --- |
| **Work Contract** | What exactly are we trying to establish, with what authority and budgets? | this document |
| **Evidence Ledger** | What do we know, how do we know it, and is it still true? | [`evidence-ledger.md`](evidence-ledger.md) |
| **Blockage Classifier** | Why is the work not progressing, and what may happen next? | [`blockage-classifier.md`](blockage-classifier.md) |
| **Micro-step Proposal Gate** | How do we shrink the model's next job without widening its authority? | [`micro-step-proposal-gate.md`](micro-step-proposal-gate.md) |

The backlog that implements them is [`coding-agent-production-roadmap.md`](coding-agent-production-roadmap.md)
(tickets RIC-01 to RIC-07). The audit that motivated them is
[`hostile-autonomous-agent-audit.md`](hostile-autonomous-agent-audit.md) (findings HA-xx).

**Principle.** Chip should keep reducing the size of the next inference problem until the model in hand
can solve it, and use independent evidence, never the model's confidence, to decide whether work is
done, needs another small step, or genuinely needs more intelligence or a human's judgment.

> **A stronger model is not a substitute for missing acceptance criteria, authority, or trustworthy
> evidence.** Only a classified `reasoning_insufficiency` outcome may trigger stronger-model escalation.
> Every other classification leads to its own recovery action or to a safe stop.

Words MUST, MUST NOT, SHOULD and MAY are used as in RFC 2119. Each requirement has an identifier
(`WC-nn`) and an **Acceptance** condition that a test can decide.

## 0. Scope and relation to what exists

Chip today builds a `WorkSpec` (`chip-core/src/work.rs`: id, goal, `WorkLimits`, required
observations, answer predicates, invariants, `evidence_reuse_prohibited`, `context_budget_bytes`) from
a goal kind (`change`, `verify`, `inspect`; `chip-cli/src/software_work.rs`) and runs it. The Work
Contract is the **explicit, versioned, auditable form of that same information, extended** with
acceptance state, per-capability budgets, candidates and provenance. It is **compiled into** a
`WorkSpec` for the existing loop; it is not a parallel runtime.

* Chip owns the contract and every decision derived from it. FX (`fx-core`, `fx-provider-http`) knows
  nothing of it. PAX supplies independent test results and structure facts; it never sees the contract.
* No persistence, global memory or orchestration framework is introduced. A contract lives in memory
  for the life of one work and is exported in the result and events.
* This PR changes no code and no public API. Section 12 lists what an implementation changes.

## 1. Shape

Canonical serialization is JSON with sorted keys and no insignificant whitespace; `contract_hash` is the
SHA-256 of that form. Unknown fields MUST be rejected by the runtime when reading a contract.

```jsonc
{
  "schema": "chip.work-contract.v1",
  "work_id": "work_…",                       // Chip-issued, never client- or model-supplied
  "version": 1,                              // integer, starts at 1, +1 per accepted material delta
  "goal": { "text": "…≤2000 bytes", "kind": "change|verify|inspect", "goal_hash": "sha256:…" },
  "acceptance": {
    "state": "ready | clarification_required",
    "basis": "submitter_criteria | failing_baseline_tests | existing_tests_unchanged | grounded_answer",
    "criteria": [ { "id": "AC1", "text": "…", "check": { "type": "pax_test | pax_test_target | observation_predicate | answer_grounded", "params": {} },
                    "required": true, "criteria_hash": "sha256:…" } ],
    "open_questions": [ { "id": "Q1", "text": "…≤400 bytes", "refs": ["AC1"] } ]      // present iff state = clarification_required
  },
  "verification": {                          // what evidence a completion decision needs (WC-17)
    "required": [ { "id": "V1", "supports": ["AC1"], "type": "pax_test", "scope": "project",
                    "freshness": "after_last_relevant_write", "test_surface": "protect_existing | may_change_with_authority" } ]
  },
  "authority": {
    "capabilities": [ { "id": "project.write", "class": "write", "budget": { "max_calls": 8, "max_bytes": 262144 } } ],
    "write_scope": "any_project_path | candidates_only",
    "trust": { "repository": "trusted | untrusted | unknown", "project_tooling": "allowed | requires_isolation | refused" }
  },
  "budgets": {
    "turns": 12, "executions": 8, "attempts": 3,
    "context": { "max_request_bytes": 65536, "max_total_bytes": 1048576 },
    "time": { "work_seconds": 1800, "model_call_seconds": 120, "verification_seconds": 300 },
    "reask": 2, "clarification_requests": 1,
    "escalation": { "stronger_model_handoffs": 0 }
  },
  "termination": [ { "on": "budget_exhausted | authority_violation | integrity_failure | clarification_required | cancelled | top_tier_exhausted", "state": "limit_reached | blocked | escalated | failed", "reason_code": "…" } ],
  "candidates": { "files": [ { "path": "src/money.rs", "sha256": "…", "reason": "failing_test_reference", "rank": 1 } ],
                  "symbols": [ { "name": "parse", "path": "src/money.rs", "kind": "fn" } ], "derived_from": ["ev-3", "ev-5"] },
  "provenance": { "created_by": "runtime", "inputs": { "chip_version": "…", "pax_version": "…", "config_hash": "…", "repo_fingerprint": "sha256:…", "goal_hash": "…" },
                  "pre_model_steps": ["baseline_verification", "deterministic_inspection"] },
  "history": [ { "version": 2, "delta": [ { "path": "/budgets/executions", "old": 8, "new": 12 } ], "reason_code": "…", "reason": "…",
                  "authority": { "kind": "runtime_policy | submitter | human_decision", "ref": "…" }, "proposed_by": "model | runtime | submitter",
                  "validated_by": ["WC-14"], "created_at_seq": 41 } ]
}
```

Defaults for every budget and capability come from the existing product defaults
(`WorkLimits` 12 turns / 8 executions for `chip work` and `serve`, ceiling 50; the declared
capability surface of `capabilities.md`). The contract makes them explicit and bound to the work.

## 2. Lifecycle

```text
          (goal + config + repository)
                     │  deterministic pre-model evaluation (section 10)
                     ▼
                  Draft ──────────────► ClarificationRequired ──(human_decision delta)──► Ready
                     │ acceptance reliable                                                   │
                     ▼                                                                       │
                   Ready ◄───────────────────────────────────────────────────────────────────┘
                     │ first model attempt binds version v
                     ▼
                   Active (versions v, v+1, … by accepted material deltas)
                     │
                     ▼
             Terminal (completed | blocked | escalated | limit_reached | failed)
```

Draft is never visible to a model. A model call is made only against a `Ready` or `Active` contract.
`ClarificationRequired` is a waiting state that ends the work as `escalated` **to a human** (the existing terminal
state meaning "a person must decide"; no new `TerminalState` is introduced) with
`reason_code = clarification_required` and a typed `open_questions` payload for the submitter. It is never
addressed to a model tier (WC-07), and it agrees with the classifier's rule BC-R2
([`blockage-classifier.md`](blockage-classifier.md) section 8).

## 3. Requirements

### 3.1 Immutability and authorship

* **WC-01.** A contract version MUST be immutable once created. The only way to change a contract is to
  create version *n+1*; version *n* remains readable for the life of the work.
  *Acceptance:* a test that obtains a version, attempts every mutation path the API offers, and finds
  the stored bytes (and `contract_hash`) of that version unchanged; the type offers no `&mut` access to a
  stored version.
* **WC-02.** The model MUST NOT create or modify an authoritative contract. A model reply is never
  parsed as contract content, and no decision kind of `chip.work-decision.v1` or `chip.micro-step.v1`
  carries contract fields.
  *Acceptance:* a scripted model that places `acceptance`, `budgets` or `authority` members in any reply
  has the reply rejected by the existing strict parser; the contract hash is unchanged.
* **WC-03.** The model MAY propose a *delta* only through the delta channel of section 5; the runtime
  validates it against policy and decides.
  *Acceptance:* tests per row of the delta table (section 5) showing accept/reject exactly as specified.
* **WC-04.** Every accepted material delta MUST create a new version and record old value, new value,
  reason, proposer, validators and authority (section 5 `history` entry).
  *Acceptance:* after a scripted accepted delta, `history` has one entry whose old/new values equal the
  diff of the two canonical contracts, and `version` has advanced by exactly one.
* **WC-05.** Changes to acceptance criteria, authority, verification requirements, failure/termination
  conditions or budgets (material changes, section 5) MUST NOT occur silently: each emits a
  `ContractAmended` event and appears in the result.
  *Acceptance:* a trace-level test over every delta class asserts the event and the result entry exist
  and that no path alters a material field without them.

### 3.2 Acceptance state

* **WC-06.** The runtime MUST establish a reliable acceptance predicate before the first model call.
  A predicate is *reliable* iff at least one required criterion has a runtime-evaluable check **and**
  that check's outcome is, before the work, observably different from "done" (it is currently unmet), or the
  goal kind is `verify`/`inspect` where "done" is defined by the kind (section 4).
  *Acceptance:* the decision table of section 4 as a table-driven test: each row's inputs produce the
  stated `acceptance.state` and `basis`.
* **WC-07.** If no reliable predicate can be established, the runtime MUST set
  `acceptance.state = clarification_required` with at least one specific `open_question`. In that state the
  work MUST NOT complete successfully, MUST NOT make a model call that can change the repository, and
  MUST NOT escalate to a stronger model **as a substitute for clarification**.
  *Acceptance:* a green-baseline `change` goal with no submitter criteria ends `escalated`
  (to a human) with reason `clarification_required`, with zero capability executions other than the pre-model inspection, zero
  stronger-model handoffs, and `open_questions` non-empty; a variant that scripts a stronger tier shows it
  is never called.
* **WC-08.** The runtime MUST NOT invent acceptance criteria to make a task executable. Criteria come from
  the submitter, from deterministic inspection of the repository (failing baseline tests), or from the
  goal kind's definition; the `basis` field records which. Text the model produced is never a source.
  *Acceptance:* every criterion in a contract has `basis`-consistent provenance in `provenance.pre_model_steps`;
  a test that feeds goal text asserting a behavior ("add X") with a green baseline observes
  `clarification_required`, not a synthesized criterion.
* **WC-09.** A `clarification_required` contract becomes `Ready` only through a version whose delta carries
  `authority.kind = human_decision` (or `submitter`) and whose criteria satisfy WC-06.
  *Acceptance:* an attempt to resolve it with `proposed_by = model` is rejected with the rule id; a
  submitter-supplied criterion produces version 2 `Ready` with the old/new acceptance recorded.

### 3.3 Binding

* **WC-10.** Every model attempt, capability execution and completion decision MUST record the contract
  version it relied on (`contract_version` on `ModelCalled`, `ExecutionRequested`, `GoalEvaluated` events
  and on the corresponding ledger records).
  *Acceptance:* a test over a multi-version run asserts that no event of those kinds lacks a version and
  that the version equals the one in force when the event was produced.
* **WC-11.** A completion decision MUST be evaluated against the *current* contract version using evidence
  eligible under it (the ledger's rules). Evidence produced under an earlier version remains eligible only
  if the `criteria_hash` of every criterion it supports is unchanged.
  *Acceptance:* a run where a material acceptance delta lands after a passing verification does not complete
  until fresh evidence exists under the new `criteria_hash`; a run where only a non-material delta lands
  completes on the earlier evidence.

### 3.4 Authority, trust and budgets

* **WC-12.** The set of capabilities in `authority.capabilities` MUST be a subset of the capabilities the
  environment declares (`CapabilitySet`); a request outside it is rejected before execution (as invalid
  invocation today). Per-capability budgets (`max_calls`, `max_bytes`) are enforced by the runtime, and
  exhaustion is an observation and a classifier input, not a silent denial.
  *Acceptance:* a scripted model that exceeds a per-capability budget is refused, the refusal is recorded
  as authority evidence, and the audit counts no unauthorized execution.
* **WC-13.** `authority.trust` MUST be set before the first model call. If project tooling would run
  (`pax.test`) in a repository whose `trust.repository` is not `trusted` and the environment is not an
  isolating one, `project_tooling` MUST be `refused` or `requires_isolation`, and the work MUST NOT start
  tooling without an explicit submitter trust decision recorded as a delta. *(Closes the design gap behind
  audit finding HA-01; the isolation mechanism itself is not specified here.)*
  *Acceptance:* the HA-01 fixtures (`build.rs`, rustc wrapper) do not execute in an untrusted, non-isolated
  run; with an explicit trust delta they do, and the history records who authorized it.
* **WC-14.** Budgets MUST only change by delta (section 5). Raising any budget or widening authority
  requires `authority.kind ∈ {submitter, human_decision}`; the model's proposals for either are rejected.
  *Acceptance:* delta-table tests (section 5).
* **WC-15.** Every budget in section 1 has a defined exhaustion behavior in `termination` and MUST be
  enforced by the runtime before the model is called again; exhaustion is reported with the budget name and
  the amount consumed.
  *Acceptance:* one test per budget drives it to zero and asserts the terminal state, reason code and that
  no further model call occurs.

### 3.5 Candidates and verification requirements

* **WC-16.** `candidates` MUST be derived deterministically (failing-test file references from the baseline
  verification, `project.observe`/`project.search` facts, file and symbol names mentioned in the goal that
  exist in the tree), with each entry citing the ledger evidence it came from. Candidates are *advisory
  context selection*; they confer no authority unless `write_scope = candidates_only`.
  *Acceptance:* the same repository, goal and configuration yield byte-identical `candidates` on repeated
  runs; a candidate without a `derived_from` evidence id fails contract validation.
* **WC-17.** `verification.required` MUST name, for every required criterion, the independent evidence that
  establishes it, its freshness rule, and the `test_surface` policy. The default for `change` work is
  `protect_existing`: a change to the set of existing tests (removed, ignored, or modified) is a conflict
  unless a `human_decision` delta authorizes it. *(Closes the design gap behind HA-02.)*
  *Acceptance:* the HA-02 fixtures (`v3a`, `v3b`, `v3c`) do not end `completed` under `protect_existing`.
  The same edit with an authorizing delta completes and records the authorization.
* **WC-18.** A completion MUST report the acceptance `basis` it was verified against (`verified_against`), and MUST
  NOT describe an outcome as the *goal* being satisfied when the basis is `failing_baseline_tests` or
  `existing_tests_unchanged`: those establish "the tests that existed, or that failed at baseline, now pass", not that
  everything the goal text asked for exists. Only `submitter_criteria` (and, for `inspect`, `grounded_answer` with its own
  limit) supports a goal-level statement.
  *Acceptance:* result JSON for a completion on a `failing_baseline_tests` or `existing_tests_unchanged` basis carries
  `verified_against` with that value and a `goal_level: false` marker; the result vocabulary distinguishes it from
  `submitter_criteria`. For the audit scenarios `v6` (requested feature never written) and `v7` (visible test special-cased)
  without submitter criteria the run may still complete, but is labelled `verified_against: failing_baseline_tests,
  goal_level: false`; with a submitter criterion that exercises the feature (or the general behavior) it is not verified
  until that criterion's check passes.

### 3.6 Versioning of the schema

* **WC-19.** The schema is versioned (`chip.work-contract.v1`). Within a major version, only additive,
  optional fields are allowed; a reader MUST refuse a contract with a higher major version or an unknown
  required field.
  *Acceptance:* round-trip tests with an added optional field (accepted), an unknown required field and a
  `v2` schema string (both rejected).
* **WC-20.** `contract_hash` MUST be computed over the canonical form and included in every record that binds
  a version.
  *Acceptance:* two serializations of the same contract in different key orders hash identically; any one-byte
  change changes the hash.

## 4. Deriving `acceptance` (decision table)

Evaluated by the runtime in this order before the first model call. "Baseline" is the pre-model
verification of section 10.

| # | Goal kind | Submitter criteria | Baseline | Result |
| --- | --- | --- | --- | --- |
| 1 | `change` | present and runtime-evaluable | any | `ready`, basis `submitter_criteria` |
| 2 | `change` | absent | `failed` with identifiable failing tests | `ready`, basis `failing_baseline_tests`: AC = "those tests pass, no previously passing test fails, test surface unchanged". It does **not** cover behavior the goal text asks for beyond those tests, and the result says so (WC-18) |
| 3 | `change` | absent | `passed` (green) | `clarification_required`: "no observable difference between done and not done; supply a failing test, a check, or an acceptance statement" |
| 4 | `change` | absent | `not_run`, `error`, `unsupported`, `ambiguous` | `clarification_required` (verification cannot establish anything) |
| 5 | `verify` | n/a | any | `ready`, basis `existing_tests_unchanged`: AC = "tests pass with no repository change" |
| 6 | `inspect` | n/a | not required | `ready`, basis `grounded_answer`; the best achievable outcome is `grounded`, never `verified` (as today) |
| 7 | any | present but not runtime-evaluable (free text) | any | `clarification_required` unless it is attached to a row-1 evaluable check; free text is never an AC |

Rollout (an implementation concern recorded here so behavior is never changed silently): rows 3, 4 and 7 first
run in **`warn`** mode: the work proceeds as today, the contract records `acceptance.state = ready` with
`weak: true` and `basis = existing_tests_unchanged`, and the result carries `acceptance_weak: true`. Switching
to **`enforce`** is a separate, announced, breaking change. *Acceptance:* both modes have tests; the mode is
printed in the result.

## 5. Deltas

A delta is a set of JSON-pointer changes to a contract. **Material** fields: `acceptance`, `verification`,
`authority`, `budgets`, `termination`. **Non-material** fields: `candidates`, `provenance.pre_model_steps`
annotations.

| Delta | Proposer | Needed authority | Runtime validation |
| --- | --- | --- | --- |
| Add/refresh candidate file or symbol | model, runtime | `runtime_policy` | path exists, is project-relative, not reserved; cites evidence; non-material (new version only if the candidate set hash changes) |
| Narrow authority (drop a capability, lower a budget) | model, runtime | `runtime_policy` | strictly narrowing; recorded as material |
| Declare a criterion unsatisfiable / ambiguous | model | none (it is a *report*, not an edit) | becomes a classifier input (`ambiguous_intent` rules), never edits `acceptance` |
| Add a criterion | submitter, human | `submitter` / `human_decision` | must satisfy WC-06; strengthening only unless authorized |
| Remove or weaken a criterion | submitter, human | `human_decision` | records the old criterion; `criteria_hash` changes so older evidence stops counting |
| Resolve `clarification_required` | submitter, human | `human_decision` | WC-09 |
| Raise a budget (turns, executions, attempts, context, time, escalation) | submitter, human | `submitter` / `human_decision` | bounded by product ceilings; ceilings are not contract fields |
| Widen authority (add a capability, `write_scope`, trust decision, authorize test-surface change) | human | `human_decision` | never from the model; trust decisions name the repository fingerprint |
| Anything proposed by the model that is not in the first three rows | model | n/a | **rejected**, with rule id, counted in `reask` budget |

*Acceptance (WC-03, WC-04, WC-14):* each row has an accept test and each rejection has a reject test asserting
the contract hash is unchanged and the rejection is a recorded observation.

## 6. What a contract is not

Not a plan (it contains no ordered steps), not memory (it holds no conversation), not a store (no
persistence), not model-readable authority (a model sees a *rendering* of the current version's
outstanding criteria, candidates and remaining budgets, never a writable copy), and not an
orchestration framework: the runtime remains the existing bounded loop, reading its limits from the
contract.

## 7. Events

The existing `WorkEvent` stream gains `ContractCreated { version, contract_hash }`,
`ContractAmended { from, to, delta_summary, authority }` and a `contract_version` field on the events of
WC-10. Existing consumers ignoring unknown event kinds are unaffected; the field additions are additive.
*Acceptance:* the existing event-order and audit tests pass unchanged with the new events filtered out.

## 8. Failure and termination

`termination` is evaluated by the runtime before every model call and after every execution. Every
budget-exhaustion and policy trigger maps to an existing terminal state (`limit_reached`, `blocked`,
`escalated`, `failed`) plus a machine-readable `reason_code`. A trigger fires at most once; the first to
fire decides the state. *Acceptance:* a property test over interleaved triggers shows exactly one terminal
state and that the reason code is the first trigger in evaluation order.

## 9. Interaction with the other contracts

* The **Evidence Ledger** supplies the facts a contract's `verification.required` is checked against, and
  records `contract_version` on every record.
* The **Blockage Classifier** reads the contract (acceptance state, budgets, authority) and ledger; its
  `ambiguous_intent` and `authority_or_capability_gap` outputs are the only routes by which a person is
  asked for a delta.
* The **Micro-step Gate** reads the outstanding criteria and allowed capabilities and can only propose
  requests inside them.
* **Stronger-model handoff** requires `budgets.escalation.stronger_model_handoffs > 0` and a classifier
  result of `reasoning_insufficiency`; a contract in `clarification_required` can never satisfy it (WC-07).

## 10. Deterministic pre-model evaluation

Before the first model call the runtime MAY run, within the contract's `time.verification_seconds` and
`executions` budgets: (1) a **baseline verification** (`pax.test`) to establish whether the project is
red, green or unverifiable, and to bind the baseline test surface and repository fingerprint into the
ledger (evidence type `verification.pax_test`, `phase = baseline`); (2) **deterministic inspection**
(`project.observe`, `project.search` on names taken from failing tests and from the goal) to derive
`candidates`. Rules:

* **WC-21.** Pre-model steps use the same capabilities, policy checks, recording and audit as model-driven
  steps; they are not a privileged path. Their results are ledger records like any other.
  *Acceptance:* an audit run counts pre-model executions under the same invariants; a hostile fixture is
  refused by the same path checks.
* **WC-22.** Pre-model steps MUST respect WC-13 (no tooling in an untrusted, non-isolated repository
  without a trust decision) and are skipped for `inspect` work.
  *Acceptance:* HA-01 fixtures do not execute before the first model call.
* **WC-23.** Pre-model evaluation is bounded: at most one baseline verification and a fixed number of
  inspection executions, all counted against the contract's budgets and reported (`pre_model_steps`,
  executions used, time used). *Performance invariant:* it must not run when its result cannot change the
  contract (`verify` kind with no candidates needed).
  *Acceptance:* a run reports pre-model cost; a `verify` run starts exactly one verification.

## 11. Non-goals

Persisting contracts, resuming a contract after a process restart (P3-01), cross-project contract
memory, model-authored plans, a general policy language, changing FX or PAX, new capabilities, and any
claim about local-model completion rates. The 85 % local-model figure that motivated this design is an
**evaluation goal** (RIC-07), not an acceptance criterion of any ticket before a reproducible real-model
baseline exists.

## 12. What an implementation changes (for estimating, not for this PR)

`chip-core`: a `WorkContract` type with canonical serialization and `compile_to_spec`; contract events;
version binding on existing events. `chip-cli`: contract construction in `software_work.rs`/`verify.rs`/
`service.rs` (including the pre-model evaluation), result fields (`contract`, `acceptance_basis`,
`verified_against`), and flags for the submitter's acceptance check and trust decision. `chip-pax`:
nothing required; `project.observe` test facts improve `test_surface` (see the ledger). FX: nothing.
Public API: additive (`WorkSpec` gains a constructor from a contract; `Capacity`-style struct-literal
breakage MUST be avoided by `#[non_exhaustive]` or builders, per audit finding HA-20).
