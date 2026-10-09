# Micro-step Proposal Gate (`chip.micro-step.v1`)

**Status: normative design. Not implemented, and not the default when it is.** See
[`work-contract.md`](work-contract.md) for the set of four contracts. This one answers: *how do we shrink the
model's next job without widening its authority?*

The gate is a **narrowing**: an optional mode in which the runtime asks the model for exactly one smallest next
capability request that advances an outstanding acceptance criterion. It is not a planner, a new decision
protocol with more power, or a way to move policy or verification into the model. **Everything the gate permits is
a subset of what the normal path permits, validated by the same checks, executed by the same code, recorded in
the same ledger, and judged by the same completion rules.**

> A stronger model is not a substitute for missing acceptance criteria, authority, or trustworthy evidence. The
> gate helps a model that *could* do the job if the problem were smaller; it never converts a missing criterion, a
> missing permission or a conflict into something a model can talk its way past.

Words MUST/SHOULD/MAY as in RFC 2119. Requirements are `MS-nn`, each with an **Acceptance** condition.

## 0. Relation to what exists

Today every model call is a free-form `chip.work-decision.v1` decision (`request_capability`, `complete`,
`escalate`, `block`) over the whole context (`ModelDecisionBoundary`, strict parse, no repair). The audit showed
the free-form form costs a malformed or off-protocol reply the whole run (HA-06), grows context without bound
(HA-07), and gives a model that is already failing no help narrowing its task (HA-09). The gate addresses the
*size of the problem*; it leaves the *authority* of the decision unchanged.

## 1. When the gate applies (mode selection)

The **runtime** chooses the mode. The model cannot request it, decline it, or detect it from anything but the
prompt it receives.

* **MS-01.** The gate MAY be used only for a call that is not the first attempt of the work, and only when at least
  one of these deterministic conditions holds: (a) the [classifier](blockage-classifier.md) returned `R6`
  (`progress_possible_with_strategy_change`) with `next_action.params.mode = micro_step`; (b) the previous call in
  the same attempt was a protocol failure (invalid, empty, or non-conforming reply) and re-ask budget remains;
  (c) classifier rule `R4` (`missing_evidence`) has no runtime-schedulable producer and the model must choose
  which read to make. Mode selection is a pure function of contract, ledger and budgets, recorded as a
  `StepMode { mode, reason_rule_id }` event.
  *Acceptance:* the first model call of every scripted work is free-form; each of (a), (b), (c) selects the gate in
  a table-driven test; a scripted model that asks for or refuses the mode changes nothing; the event is recorded.
* **MS-02.** The gate is **off by default** until measurement (section 7) justifies a default. It is enabled by a
  submitter-visible setting recorded in the contract (`budgets`/`policy` field) and in the result.
  *Acceptance:* with the setting off, no scripted trajectory ever enters the gate (zero `StepMode` events with
  `micro_step`).

## 2. What the model is asked and what it may answer

The request is built by the runtime from the **contract and the ledger** (the projection of
`evidence-ledger.md` EL-17), not from transcript text, and contains: the outstanding criteria (ids, text, current
evidence status), the candidate files, the strategies already tried and their outcomes, the remaining budgets, and
a **closed list of allowed capabilities for this step** = (the contract's capabilities with budget left) ∩
(the capability classes plausible for the outstanding evidence need, chosen by the runtime). It asks for exactly
one JSON object.

```jsonc
// Form A: exactly one candidate request
{ "schema": "chip.micro-step.v1",
  "advances": "AC1",                                  // an outstanding criterion id of the current contract version
  "request": { "capability": "project.read", "inputs": { "path": "src/money.rs", "offset": 0, "length": 2000 } },
  "rationale": "≤200 bytes, display only" }

// Form B: exactly one structured blocked reason
{ "schema": "chip.micro-step.v1",
  "blocked": { "code": "no_candidate_found | needs_authority | needs_clarification | conflicting_evidence | cannot_determine_next_step",
               "refs": ["AC1", "ev-14"],              // criterion and/or ledger ids, validated to exist
               "detail": "≤300 bytes, display only" } }
```

* **MS-03.** The response MUST be exactly Form A or exactly Form B. Any other shape is invalid: unknown fields,
  more than one `request`, an array of steps, a `complete`, `escalate` or `block` decision, a contract delta, a
  verification claim, a plan, or prose around the object. The parser is as strict as `chip.work-decision.v1` (no
  repair; the existing single-code-fence tolerance applies identically or not at all, decided once for both).
  *Acceptance:* a corpus of malformed and over-reaching replies (including each listed shape) is rejected by the
  parser with a typed error; none reaches validation.
* **MS-04.** `rationale` and `detail` are display-only. They MUST NOT be parsed for authority, MUST NOT be stored
  as evidence, and enter the ledger only as `claim.model`.
  *Acceptance:* a rationale claiming "tests pass" or "you are authorized" changes no state; the ledger has a
  `claim.model` record and nothing eligible.

## 3. Validation (the runtime, not the model)

For Form A (V1 to V9) and Form B (V1 and V10), all of the applicable checks MUST hold, or the reply is **rejected, not executed**, and counts against the
re-ask budget:

| # | Check | Source of truth |
| --- | --- | --- |
| V1 | structurally valid per MS-03 | parser |
| V2 | `advances` names an outstanding (required, not-yet-fresh-satisfied) criterion of the **current contract version** | contract + ledger |
| V3 | `capability` is in the step's closed list and in the contract's `authority.capabilities` with budget remaining | contract |
| V4 | the capability's own input rules accept the inputs (path rules, sizes, reserved names, symlinks) | existing invocation boundary, unchanged |
| V5 | if `write_scope = candidates_only`, a write targets a candidate path | contract |
| V6 | if the request writes, it carries the content hash of the version the model last observed (EL-11) | ledger |
| V7 | the request is not a repeat of an earlier request that failed with unchanged relevant evidence (same `strategy_fingerprint`) | ledger |
| V8 | size bounds: `rationale` ≤ 200 B, `detail` ≤ 300 B, reply ≤ the contract's `max_reply_bytes` | contract |
| V9 | the trust rules of WC-13 still permit it (a tooling request in an untrusted, non-isolated repository is refused) | contract |
| V10 | (Form B) every id in `blocked.refs` names an existing criterion of the current contract version or an existing ledger record | contract + ledger |

* **MS-05.** The gate MUST add no permission: any request accepted by the gate would be accepted by the normal path
  under the same contract, and any request the normal path refuses is refused by the gate with the same reason.
  *Acceptance:* a differential test replays a corpus of requests (including every audit attack: outside-root write,
  absolute path, `.env`, `.git`, `shell.exec`, `build.rs` under an untrusted repository) through both paths and
  asserts identical accept/refuse outcomes; the gate is never more permissive.
* **MS-06.** The gate MUST NOT let the model expand authority, rewrite acceptance criteria, bypass verification, or
  issue an unbounded multi-step plan. Concretely: `advances` can only select among existing criteria (never add or
  change one); there is no field that names a budget, capability grant or verification result; one reply yields at
  most one execution; and the next step requires a new, validated call.
  *Acceptance:* four adversarial scripts (try to add a criterion, to claim a verification, to request two actions,
  to ask for more budget) are each rejected at parse or validation with the contract hash unchanged.

## 4. Execution, evidence and completion are unchanged

* **MS-07.** An accepted request is executed by the same `CapabilitySet` path as any request: the same audit
  invariants run, the observation is recorded in the ledger with `produced_by` and `contract.version`, and
  dependency and invalidation rules apply (EL-04 to EL-07).
  *Acceptance:* the audit counters (`out_of_root_write`, `path_escape`, `unauthorized_executions`, …) are computed
  over gate-mode runs and are zero for the scripted attacks; ledger records for gate steps are indistinguishable
  from normal ones except `step_mode`.
* **MS-08.** The model cannot complete the work in this mode. There is no `complete` in Form A or B. Completion is
  decided by the runtime from the contract and ledger after every accepted step (the existing behavior for
  `change`/`verify` goals, extended by EL-14), so verification can neither be skipped nor claimed.
  *Acceptance:* a scripted model that writes a plausible fix and never asks for verification still ends only when
  the runtime schedules fresh verification (classifier R4) and it passes; a model that could "claim" completion has
  no way to express it.
* **MS-09.** Form B (`blocked`) feeds the classifier as a **hint** (S11) and never terminates the work by itself.
  `needs_authority` is treated as `authority_or_capability_gap` only if a runtime refusal (S5) corroborates it;
  `needs_clarification` goes through the specificity check of BC-12; `conflicting_evidence` must cite existing
  evidence ids that the ledger confirms conflict; `no_candidate_found` and `cannot_determine_next_step` count
  toward `reasoning_insufficiency` basis 2 only if no infrastructure faults occurred.
  *Acceptance:* one test per code: an uncorroborated `needs_authority` is ignored; a corroborated one yields R3; a
  fabricated evidence reference in `conflicting_evidence` is rejected at validation (V10).

## 5. Budgets and loop guards

* **MS-10.** Every gate call consumes a turn, a model-call and (if accepted) an execution; a rejected reply
  consumes the re-ask budget (`budgets.reask`). When the re-ask budget is spent the classifier is consulted (R7
  basis 2 or R5/R8), not the gate again.
  *Acceptance:* a script of consecutive invalid replies stops after exactly `reask` rejections and the classifier
  output is as the corpus states.
* **MS-11.** The gate MUST NOT run unbounded: at most `micro_step_max_calls` (contract, default 8) per outstanding
  criterion per attempt.
  *Acceptance:* a script that always returns a valid, harmless read stops at the cap with a classifier decision.

## 6. Interaction with the other contracts (summary)

| With | Rule |
| --- | --- |
| Work Contract | reads outstanding criteria, authority, budgets; may not propose deltas (WC-02) |
| Evidence Ledger | context is a projection of it (EL-17); steps are recorded as ordinary records |
| Blockage Classifier | selects the mode (R6/R4); consumes Form B as a hint; its R7 basis 2 requires the gate to have been tried |
| FX | unchanged: the gate is a different prompt and parser; providers see an ordinary request |
| PAX | unchanged: verification evidence still comes from PAX and is interpreted by it |

## 7. Measurement before it can become a default

The gate is a hypothesis: a smaller next problem raises the rate of **independently verified** completion and
reduces wasted inference for models that fail free-form. It is not adopted by argument.

* **MS-12.** The gate MUST NOT become the default, nor be described as improving outcomes, until an A/B evaluation on
  a fixed task suite shows it. The evaluation (owned by ticket RIC-07, run with a real model) uses: arms
  *free-form* vs *gate-on-eligible-calls*, same tasks, same contract, same budgets; the primary metric is
  **independently verified completion rate** (acceptance checks the model cannot edit); secondary metrics are
  regression rate, wasted inference (model calls that change neither the repository nor the ledger's evidence
  status, plus rejected replies), tokens and wall time per verified completion, and classifier decisions; at least
  *N* repetitions per task (to be fixed in the evaluation plan before the first run) so that variance is reported.
  *Acceptance:* the evaluation plan document exists and fixes the suite, *N* and the decision rule **before** any
  run; results are published with scripted and real runs separated; the default changes only by a reviewed PR
  citing the results.
* **MS-13.** The 85 % local-model target that motivated this design is an **evaluation goal** (measured on the fixed
  suite once a reproducible real-local-model baseline exists), not an implementation acceptance criterion of this
  gate or of any ticket before that baseline.
  *Acceptance:* no ticket's acceptance criteria cite the figure; the evaluation plan does.

## 8. Failure modes the design guards against

| Risk | Guard |
| --- | --- |
| Gate becomes a permission bypass | MS-05 (differential test), V3/V4/V9 reuse existing checks |
| Model uses `advances` to redefine "done" | MS-06; `advances` selects among existing criteria only |
| Model uses `blocked` to stall or to get a free human | MS-09, BC-12 (specificity, budget); worst case one bounded question |
| Gate hides verification | MS-08: no completion claim exists; runtime schedules verification |
| Gate loops on cheap reads | MS-10, MS-11, V7 |
| Gate prompt leaks stale facts | built from the ledger projection (EL-17), stale records cannot appear as current |

## 9. Non-goals

Plans, multi-step proposals, sub-agents, tool-use loops inside one reply, model-chosen modes, model-visible
contract editing, a new capability class, changing FX or PAX, and any claim about performance before MS-12.
