# Deterministic Blockage Classifier

**Status: normative design. Not implemented.** See [`work-contract.md`](work-contract.md) for the set of
four contracts. This one answers: *why is the work not progressing, and what may happen next?*

> A stronger model is not a substitute for missing acceptance criteria, authority, or trustworthy
> evidence. **Only `reasoning_insufficiency` is eligible to trigger a stronger-model handoff.** Every
> other classification leads to its own recovery action or to a safe stop.

The classifier is a **pure function**. It does not call a model, read a clock, use randomness, or
consult any model's identity. It must not infer reasoning insufficiency from task complexity, from the
model's name or size, or from an unsuccessful first attempt.

Words MUST/SHOULD/MAY as in RFC 2119. Requirements are `BC-nn`, each with an **Acceptance** condition.

## 0. Where it sits

Today the product has terminal states (`completed`, `escalated`, `blocked`, `limit_reached`, `failed`),
turn and execution limits, and a model that may emit `block` or `escalate`; nothing decides *why* work
stalled (audit finding HA-10), and the repetition guard lives only in the evaluation harness (HA-09). The
classifier turns those signals into one typed, auditable decision and a **permitted next action**. It reads
the [Work Contract](work-contract.md) and the [Evidence Ledger](evidence-ledger.md) and nothing else.

## 1. Signature

```text
classify(contract: WorkContract, ledger: Ledger, trajectory: TrajectorySummary, budgets: BudgetState)
    -> Classification

Classification {
  class:        reasoning_insufficiency | missing_evidence | repeated_execution_failure |
                authority_or_capability_gap | ambiguous_intent | verification_conflict |
                progress_possible_with_strategy_change,
  rule_id:      "BC-R1" … "BC-R8",            // the rule that fired
  basis:        closed enum per class (section 4),
  evidence_ids: [ "ev-…" ],                    // the ledger records the decision rests on
  inputs_digest:"sha256:…",                    // hash of the signals of section 2, for replay
  next_action:  { kind: gather | change_strategy | clarify | authorize | preserve_and_investigate |
                         handoff_stronger_model | stop, params: {…} },
  terminal:     null | { state: blocked | escalated | limit_reached | failed, reason_code: "…" }
}
```

* **BC-01.** `classify` MUST be a total, deterministic function: for any input it returns exactly one class, and
  equal inputs (by `inputs_digest`) return equal outputs, with no model call, no clock and no randomness.
  *Acceptance:* a property test over generated trajectories asserts totality and that repeated evaluation and
  evaluation in a different process agree; a source test forbids model/clock/random imports in the module.
* **BC-02.** Exactly one class is returned. Where signals support several, the **precedence of section 3** decides.
  *Acceptance:* a table-driven corpus (section 6) in which every row has one expected class, plus overlap rows
  asserting the higher-precedence class wins.

## 2. Observable inputs (the only inputs)

All are derived from the contract, the ledger and the budget state. None is read from model prose.

| Signal | Definition |
| --- | --- |
| S1 `acceptance_state` | contract `ready` or `clarification_required` |
| S2 `open_clarification` | the contract has `open_questions`, or a validated model report of ambiguity (BC-12) is pending and the clarification budget remains |
| S3 `integrity` | `verify_ledger()` result: ok / failed / unverified |
| S4 `conflicts` | pairs of eligible-or-recent records that disagree about the same criterion; `repo.external_change` landing inside a verification (`mid_run_change`); test-surface change without authorization |
| S5 `refusals` | observations produced by runtime policy: reserved path, undeclared or unavailable capability, per-capability budget exhausted, write-scope violation, tooling refused by trust |
| S6 `evidence_gap` | required verification entries with no `Fresh` record (missing, stale or inconclusive), and whether a capability inside the contract could produce each |
| S7 `failure_signatures` | per `attempt.failure`: `strategy_fingerprint`, the normalized failure signature of its failure evidence (PAX `reason` code plus the first failing test identifiers or compiler error codes, not prose) |
| S8 `protocol` | counts of malformed/invalid replies, re-asks used, timeouts, provider errors (each separately) |
| S9 `progress` | change in the number of failing tests or in the failure signature set between consecutive attempts, taken from verification evidence |
| S10 `budgets` | remaining attempts, turns, executions, re-asks, clarification requests, escalation handoffs, time |
| S11 `model_hints` | the closed-enum `blocked.code` of a micro-step reply or a closed `reason_code` on a `block`/`escalate` decision; **hints are never decisive on their own**. (Today those decisions carry free text only; an implementation adds an additive, optional, closed `reason_code` to `chip.work-decision.v1`, and the classifier ignores free text) |
| S12 `tier` | whether a stronger tier is configured and the highest tier already used (configuration, not a model property) |
| S13 `microstep_tried` | whether the micro-step mode ([`micro-step-proposal-gate.md`](micro-step-proposal-gate.md)) has been used on the current outstanding criterion |

## 3. Precedence and rules

Rules are evaluated in this order; **the first that matches fires**. The order puts safety, truth and
authority before effort, and intelligence last: a stronger model is considered only when every other
explanation has been excluded.

| Order | Rule | Class | Fires when (all conditions) |
| --- | --- | --- | --- |
| 1 | BC-R1 | `verification_conflict` | S3 integrity failed or unverified for a record the decision needs; **or** S4 has a conflict (contradicting fresh records; external change inside a verification; unauthorized test-surface change) |
| 2 | BC-R2 | `ambiguous_intent` | S1 = `clarification_required`; **or** S2 open clarification (BC-12) |
| 3 | BC-R3 | `authority_or_capability_gap` | S5 has a refusal for a request that a required criterion depends on (the contract marks which capability a criterion's evidence needs) **and** no in-authority alternative remains in the strategy catalog |
| 4 | BC-R4 | `missing_evidence` | S6 shows at least one required verification without a `Fresh` record, a capability inside the contract can produce it, and it has not been attempted since its last invalidation |
| 5 | BC-R5 | `repeated_execution_failure` | the same `(strategy_fingerprint, failure signature)` has failed **at least `repeat_threshold` times** (contract; default 2) for the same criterion |
| 6 | BC-R6 | `progress_possible_with_strategy_change` | not R1 to R5, and there is evidence of progress (S9 improved) **or** an unused strategy remains in the catalog, **and** attempts/turns/executions remain |
| 7 | BC-R7 | `reasoning_insufficiency` | **all** of: S1 `ready`; S4 and S3 clean; S5 empty or irrelevant to the criterion; S6 shows no obtainable missing evidence; no repeated identical failure; and one of the *bases* of section 4.7 holds. *Whether a stronger tier exists decides the action (section 7), not the class* |
| 8 | BC-R8 | `progress_possible_with_strategy_change` (fallback, section 5) | no rule R1 to R7 fires, or the inputs are incomplete (section 5) |

* **BC-03.** The order is normative. *Acceptance:* for each pair of adjacent rules, a fixture satisfying both
  conditions asserts the earlier rule's class; the corpus of section 6 holds one such row per adjacent pair.
* **BC-04.** A rule's conditions MUST be evaluated only from the inputs of section 2.
  *Acceptance:* the module has no access to model text (type-level: its input structs contain no free-form model
  string beyond closed enums and length-bounded `refs`); a test that mutates only model prose leaves the output
  unchanged.

### 3.1 Rule detail

* **R1 `verification_conflict`.** Next action `preserve_and_investigate`: keep every conflicting record, do not
  override either, re-run the conflicting verification once at a quiescent tree (counts against executions); if the
  conflict persists or integrity stays failed, terminal `blocked`, reason `verification_conflict`. *The runtime never
  resolves a conflict in the model's favor.* **BC-05.** *Acceptance:* a scripted weakened test with no
  authorization yields R1 (not R7, not R6); the conflicting records survive in the result; a persistent conflict ends
  `blocked`.
* **R2 `ambiguous_intent`.** Next action `clarify`: terminal `escalated` **to a human** (`escalation.to = human`,
  `reason_code: ambiguous_intent`) carrying `open_questions`, the criteria they refer to, and the evidence ids
  already gathered. **No stronger-model handoff, ever.** **BC-06.** *Acceptance:* with a stronger tier configured and
  budget available, an ambiguous contract produces zero handoffs and one human-addressed question.
* **R3 `authority_or_capability_gap`.** Next action `authorize` (ask a person to grant a specific capability or
  scope, by naming it), or `stop` with `reason_code: capability_missing` when the capability does not exist in the
  environment. Terminal `escalated` (to human) or `blocked`. **BC-07.** *Acceptance:* a scripted refusal of a needed
  write path yields R3 with the capability and scope named; no handoff occurs; granting the delta (`human_decision`)
  lets the next evaluation proceed.
* **R4 `missing_evidence`.** Next action `gather`: the **runtime** schedules the cheapest producing capability
  (re-run `pax.test` after a write, re-read an invalidated candidate) before any model call; the model is not asked to
  decide whether to gather. If gathering was already attempted since the last invalidation and failed to produce
  evidence, the class falls through to a later rule (the evidence is *obtainable* no longer). **BC-08.** *Acceptance:*
  after a write that invalidates a test result, the next action is a runtime-scheduled verification with zero model
  calls in between.
* **R5 `repeated_execution_failure`.** Next action `change_strategy` if an unused strategy remains in the catalog,
  else `stop` (terminal `blocked`, `reason_code: repeated_failure`). Repeating the same strategy fingerprint on the
  same criterion after R5 fired is forbidden. **BC-09.** *Acceptance:* the audit scenario `e-repeat-identical-wrong-
  write-then-test` is classified R5 after `repeat_threshold` repeats and the loop stops or changes strategy instead
  of spending all executions (closes HA-09).
* **R6 `progress_possible_with_strategy_change`.** Next action `change_strategy` to the next catalog strategy
  (typed strategies are specified in roadmap ticket RIC-04) or to micro-step mode if not yet tried
  (`micro-step-proposal-gate.md`). Stronger models are not involved. **BC-10.** *Acceptance:* a trajectory whose
  failing-test count decreased between attempts is R6, not R7, whatever the attempt count.
* **R7 `reasoning_insufficiency`.** Next action `handoff_stronger_model` when S12 shows a configured stronger tier
  and the escalation budget is above zero (section 7); otherwise `stop` with terminal `escalated` **to a human**
  (`reason_code: reasoning_insufficiency`), because the situation is the same and only the policy differs. **BC-11.**
  *Acceptance:* only the corpus rows built to satisfy section 4.7 produce R7; flipping any one precondition (clean
  integrity, no refusal, no missing evidence, no repeat, a basis) produces another class; with no tier or no budget
  the class is still R7 and the action is the human-addressed stop, never a handoff.

## 4. Classes, bases and next actions

### 4.1 `ambiguous_intent` bases

`contract_clarification_required` (R2 via S1) or `validated_model_report` (BC-12). 

* **BC-12.** A model-reported ambiguity (a micro-step `blocked.code = needs_clarification` or a `block` with
  `reason_code = ambiguous_requirement`) is accepted as `ambiguous_intent` **only if** it passes a specificity
  check: question text 1 to 400 bytes, referencing at least one outstanding criterion id or candidate path, within
  the `clarification_requests` budget, and no `submitter_criteria` basis already makes the criterion
  runtime-evaluable. Otherwise the report is counted as a hint (S11) and ignored by R2. *The purpose is to let a
  person be asked when the model sees ambiguity the runtime cannot, without letting "ambiguous" become an
  unbounded way to stall; the worst outcome is one bounded, safe question.*
  *Acceptance:* a specific report passing the check yields R2; a vague report, a report beyond budget, and a report
  against a runtime-evaluable criterion do not.

### 4.7 `reasoning_insufficiency` bases

Exactly one of the following MUST hold, in addition to all R7 preconditions:

1. `exhausted_strategies`: at least `min_attempts` (contract; default 3) attempts have completed, using at least 2
   distinct strategy fingerprints, each with **protocol-valid, in-authority proposals** that were executed and
   rejected by independent evidence with **distinct** failure signatures, and no catalog strategy remains.
2. `protocol_incapacity_after_microstep`: the micro-step mode was tried on the criterion and the model still
   produced `re-ask budget` consecutive invalid or non-conforming replies (S8), with no provider/timeout/transport
   failures among them (those are infrastructure faults, not reasoning).

Neither basis is satisfied by: a first failed attempt, task size, file count, a long goal, a model's name, a provider
error, a timeout, a refusal, or the model's own claim that the task is hard. *Acceptance:* a fixture per excluded
condition asserts the class is not R7.

### 4.8 Next-action table

| Class | Permitted next action | Never |
| --- | --- | --- |
| `reasoning_insufficiency` | stronger-model handoff under policy (section 7); with no tier or budget, a human-addressed stop | route to a human instead of a configured tier while escalation budget remains |
| `missing_evidence` | gather it (runtime-scheduled) | ask a stronger model to guess it |
| `repeated_execution_failure` | change strategy or stop | repeat the failed strategy; escalate |
| `authority_or_capability_gap` | request authorization, report the missing capability, or stop | widen authority from model text; escalate |
| `ambiguous_intent` | ask the human a specific question | invent criteria; escalate to a model |
| `verification_conflict` | preserve and investigate; stop if unresolved | override evidence; mark complete |
| `progress_possible_with_strategy_change` | select a different bounded repair strategy | stronger model |

## 5. Conservative fallback

* **BC-13.** When the inputs are **incomplete** such that rules cannot be evaluated safely (ledger integrity
  unverified is R1; a dangling dependency is R1; signals missing because the run ended before they could be recorded
  is "incomplete"), the classifier MUST NOT guess a class that permits handoff. If the incompleteness concerns
  evidence, class `verification_conflict` (R1). If the inputs are complete and no rule fires (including a first blocked trajectory with budget remaining), the class is
  `progress_possible_with_strategy_change` (R8): `change_strategy` when attempts and budget remain, else terminal
  `limit_reached` with `reason_code: budget_exhausted`. The fallback never produces `handoff_stronger_model`.
  *Acceptance:* trajectories with a missing signal, an empty ledger or an exhausted catalog classify as above;
  none yields R7. (An exhausted catalog with a satisfied basis of section 4.7 is R7, not the fallback.)

## 6. Test corpus (normative minimum)

The corpus is a fixture directory of trajectories (contract + ledger + summary) with expected
`(class, rule_id, next_action.kind, terminal)`. It MUST include at least the following, many of which exist today as
audit scenarios (`audit/hostile`) or harness tests:

| Trajectory | Expected |
| --- | --- |
| green baseline, `change` goal, no criteria | R2 `ambiguous_intent` (contract `clarification_required`) |
| model weakens a failing test, no authorization (`v3a`) | R1 |
| ledger record altered / chain broken | R1 |
| write to reserved path refused twice, criterion needs it | R3 |
| test result invalidated by a later write | R4, runtime re-verification |
| identical failing write+test repeated (`e-repeat-…`) | R5 |
| failing test count 5 → 3 between attempts | R6 |
| 3 attempts, 2 distinct strategies, distinct failure signatures, catalog exhausted, tier configured | R7 `exhausted_strategies` |
| same, but no stronger tier configured | R7, action `stop`, terminal `escalated` to a human (never a handoff) |
| same, but a refusal in the history | R3 |
| 30 s timeouts, provider 503s | not R7 (infrastructure): `progress_possible…` with retry per the contract, then `stop` |
| micro-step tried, 2 consecutive non-conforming replies, no infra faults | R7 `protocol_incapacity_after_microstep` |
| first attempt fails | never R7 |
| a request for a nonexistent capability | R3 (`capability_missing`) |

*Acceptance:* the corpus runs in CI; adding a row requires stating its expected class; the audit scenarios named
above are the seed rows and keep their ids.

## 7. Stronger-model handoff (the only consumer of R7)

* **BC-14.** A handoff MAY occur only when (a) the current classification is R7, (b)
  `budgets.escalation.stronger_model_handoffs > 0` and a stronger tier is configured (roadmap P2-03), and (c) the
  contract is `Ready`. The payload is built **only from the contract and the ledger** (outstanding criteria,
  candidate files, eligible evidence, `attempt.failure` records with strategies already tried, remaining uncertainty
  as a list of criteria with status, and the constraint set), bounded by the context budget, and carries claims
  labeled as claims. The handoff consumes one unit of the escalation budget whether or not it succeeds.
  *Acceptance:* a handoff request body contains no transcript text, cites only ledger ids, is within budget, and the
  budget decreases; a handoff attempt under any other class is refused by the runtime and recorded.
* **BC-15.** The runtime MUST NOT assume a stronger model succeeds. After a handoff the same classifier governs;
  a second R7 with the top tier already used is `stop` (terminal `escalated` to a human with the full ledger).
  *Acceptance:* a scripted stronger tier that fails ends in a human-addressed stop, not a loop or a third tier.
* **BC-16.** The outcome of every handoff (verified result, regression, tokens, time, strategy) is a ledger record so
  that "did the stronger model help on this failure class?" can be answered from data (roadmap RIC-07).
  *Acceptance:* each handoff in a scripted run yields an `attempt.failure` or a verification record attributing it
  to the tier.

## 8. Terminal mapping (reuses existing states)

| Next action | Terminal state | `reason_code` examples |
| --- | --- | --- |
| `stop` after R5/R8/R7 with budget gone | `blocked` or `limit_reached` | `repeated_failure`, `budget_exhausted`, `top_tier_exhausted` |
| `clarify` (R2) | `escalated` (to human) | `ambiguous_intent`, `clarification_required` |
| `authorize` (R3) | `escalated` (to human) | `authority_gap` |
| `stop` (R3, missing capability) | `blocked` | `capability_missing` |
| `preserve_and_investigate` unresolved (R1) | `blocked` | `verification_conflict` |

Human-addressed terminals carry a typed payload (contract version, open questions or the capability needed, ledger
ids, what was tried). *Acceptance:* each terminal in a scripted run has the state, reason code and payload fields
listed; no new `TerminalState` variant is required.

## 9. Non-goals

Learning a classifier, model-based classification, probabilities, ranking strategies by predicted success, choosing
*which* stronger model, executing the handoff, or measuring whether handoffs help (RIC-07). The classifier names the
situation and the permitted action; the runtime enforces it.
