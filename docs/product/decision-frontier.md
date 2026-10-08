# The Decision Frontier

Status: **implemented in `chip-core`, work-local, runtime-owned.** It makes explicit what a piece of work
has left unresolved, so that "the goal is not yet met" can be told apart from "the last decision was
wrong". It is not a planner, a memory, a graph, a knowledge base or an escalation mechanism, and the
model neither writes nor maintains it.

> A goal being unsatisfied is not itself evidence that the previous decision was wrong.

## 1. What problem it removes

Chip records `GoalEvaluated(satisfied = false)` after every observation. That is a statement about the
goal, and it was also being read as a statement about the decision. A normal, successful change reads:

```text
list    -> goal unmet      (looked at the project)
read    -> goal unmet      (learned what the code does)
write   -> goal unmet      (made the change)
test    -> goal met        (verified it)
```

Read as "every unmet evaluation is a miss", that is three wrong decisions and a recovery. It is none of
those. `wrong_valid_decisions`, `first_miss` and `recovery_*` were derived that way, so they reported
progress as error. The frontier is the runtime state that says why those three steps were fine.

## 2. The model of it

```text
KNOWN      what observations, executions, evidence and goal evaluation already establish (not duplicated)
FRONTIER   unresolved questions that bear on the next safe decision (this document)
UNKNOWN    everything else (not represented)
```

```rust
DecisionFrontier { items: Vec<FrontierItem> }
FrontierItem { id: FrontierItemId /* F1, F2, ... */, question: String, kind: FrontierKind,
               status: FrontierStatus /* Open | Resolved | Invalidated */,
               resolution: Option<FrontierResolution { evidence: ExecutionId }> }
FrontierKind { MissingEvidence, Ambiguity, UnverifiedHypothesis, MissingCapability, MissingAuthority }
```

There is no score, priority, owner, dependency, timestamp, hash or persistence. A transition is grounded in
the **execution whose observation moved the item** (`ExecutionId`, the identity Chip already assigns). The
frontier refers to that evidence; it never copies it, and nothing a model said can move an item.

## 3. What moves an item

What answers a question is an `ObservationPredicate`, the machinery the runtime already uses to decide
whether observations establish an outcome. A caller declares each item with its predicate
(`WorkSpec::with_frontier_item`).

| Transition | When |
| --- | --- |
| **Opened** | at the start, for each declared item; for a successor to an invalidated item; and for a failed execution, which opens "does *capability* succeed after execution *E* failed?" (one per capability while it is open) |
| **Resolved** | the item's predicate came to hold because of this execution's observation (or, for a failure question, a later success of the same capability) |
| **Invalidated** | the item was resolved and a later observation made its predicate stop holding (a verification superseded by a later change). A successor with the same question is opened, so a stale answer never stays silently open |

A failed execution never resolves anything. `partial` is not `complete`, and `observed` is not `verified`:
a predicate sees exactly what its observation says. Resolving every item is **not** goal satisfaction,
**not** verification and **not** useful work; none of those reads the frontier.

Work that declares no items gets one derived item per required output and per required observation, and is
judged as single-step work (section 5).

### The initial frontier per kind (declared by `chip-cli`, from the kind's own completion semantics)

| Kind | Items |
| --- | --- |
| **change** | F1 *Has the project's current state been observed?* (any successful list, search, read, structure or Git observation); F2 *Has the requested change been made?* (a content-changing write); F3 *Does the changed project pass verification?* (`VerifiedChange`: pax passed after the last change) |
| **verify** | F1 *Does the unchanged project pass verification?* (`VerifiedState`) |
| **inspect** | F1 *Has the project been observed read-only, with no file changed?* (`InspectionObserved`). Whether the **answer** is grounded stays a completion gate (`GroundedAnswer`); it is not a frontier question |

## 4. What a decision did

Every executed decision is classified from the events and observations alone (`measure_utility`), never
from a counter the loop kept:

| Class | Meaning | Counts as |
| --- | --- | --- |
| **progress** | it resolved at least one item | `frontier_progress_events` |
| **supporting** | multi-step work; no item moved; the observation told the work something it had not been told (not a byte-identical repeat of an earlier observation of the same invocation) | `supporting_decisions` |
| **failed** | the execution failed | `failed_observations`; a miss |
| **invalidating** | it made an earlier answer stop holding | a miss |
| **no effect** | no item moved, and it was a repeat (or, in single-step work, it answered nothing) | `wrong_valid_decisions`; a miss |

A **miss** is a failed, invalidating or no-effect execution. **Recovery begins at the first miss**
(`first_miss`, `recovery_turns`, `recovery_executions`, `recovery_model_calls`). A step that advanced the
work or told it something new is never a miss, however far the goal still is. `recoveries` (a failure
followed by a further execution) is unchanged. `useful_work_per_*` is unchanged: it comes from `verified`
alone.

## 5. Single-step work keeps its meaning

Work whose frontier is only its requirements (a required output; a required observation) has no
intermediate questions, so an execution that answers nothing *is* a wrong valid decision. The PR38/PR39
proof harness depends on exactly that and its tests pass unchanged. Declaring items is declaring
multi-step work, where a successful step that adds information is support.

## 6. Reporting

The work result gains two compact blocks (the full frontier is not printed; the event stream has detail):

```json
"frontier":  { "opened": 3, "resolved": 3, "invalidated": 0, "remaining": 0, "progress_events": 3 },
"decisions": { "wrong_valid": 0, "supporting": 2, "recovery_turns": 0, "recovery_executions": 0, "recovery_model_calls": 0 }
```

and the audit gains `frontier_without_evidence`. Events (in the stream, in the service's `/events`, and in
`work_demo`): `FrontierOpened`, `FrontierResolved`, `FrontierInvalidated`, `FrontierProgress`, each citing
the execution it rests on. The audit checks, independently of the loop, that every transition names an
execution with a recorded observation.

## 7. Safety and boundaries

The frontier lives in `chip-core` and depends on no adapter, PAX, Compute, Attn or FeltDB; the model's
protocol is unchanged (no frontier JSON, no `resolves:` field); nothing is persisted. A model still cannot
execute, authorize, create evidence, mark anything verified or mark a goal satisfied. Existing completion
semantics (`goal_satisfied`, `grounded`, `verified`) are untouched.

## 8. L2 regression: the same trajectories, accounted both ways

Real loop, real capabilities, real PAX; only the model is scripted
(`crates/chip-cli/tests/capability_scenarios.rs`, `s1_` to `s10_`). "Old" is what the retired edge-based
reading says of the same events; "new" is the frontier accounting. Arrows are old to new.

| # | Scenario | Outcome | Calls / execs | Wrong valid | Recovery execs | Frontier opened / resolved / invalidated / remaining | Progress | Verified |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | many valid observations, then a verified change | completed | 5 / 5 | 4 -> **0** | 4 -> **0** | 3 / 3 / 0 / 0 | 3 | yes |
| 2 | intermediate steps, goal unmet | blocked | 4 / 3 | 3 -> **0** | 2 -> **0** | 3 / 1 / 0 / 2 | 1 | no |
| 3 | change then verification, no looking | completed | 2 / 2 | 1 -> **0** | 1 -> **0** | 3 / 2 / 0 / 1 | 2 | yes |
| 4 | verification failure | blocked | 4 / 3 | 3 -> **0** | 2 -> **0** | 4 / 2 / 0 / 2 | 2 | no |
| 5 | execution failure, then recovery | completed | 4 / 4 | 3 -> **0** | 3 -> 3 | 4 / 4 / 0 / 0 | 3 | yes |
| 6 | recovery after a failed verification | completed | 5 / 5 | 4 -> **0** | 4 -> **2** | 4 / 4 / 0 / 0 | 3 | yes |
| 7 | capability unavailable | failed | 1 / 0 | 0 -> 0 | 0 -> 0 | 3 / 0 / 0 / 3 | 0 | no |
| 8 | a later change invalidates a verification | blocked | 6 / 5 | 4 -> **0** | 4 -> **1** | 5 / 3 / 1 / 1 | 3 | no |
| 9 | an identical repeat | blocked | 3 / 2 | 2 -> **1** | 1 -> 0 | 3 / 1 / 0 / 2 | 1 | no |
| 10 | inspect: grounded, not verified | completed | 3 / 2 | 1 -> **0** | 0 -> 0 | 1 / 1 / 0 / 0 | 1 | no (grounded) |

False completions: 0 in every scenario. Reading the table: legitimate progress (1, 2, 3, 6, 10) is no
longer a wrong decision or a recovery; a failure still begins recovery (5, 6); an invalidation begins
recovery without any failed execution (8); and the one genuinely useless step, an identical repeat, is
still counted as wrong (9). Scenario 3 completes with one question never asked, which is the point: the
frontier is not the goal.

## 9. What this does not do

No planner, decomposition, memory, knowledge graph, repository graph, automatic observation selection,
escalation, model routing, human escalation, precedent store, persistence or new capability; no change to
PAX, Compute, FX or Attn; no new model protocol. It does not yet expose the frontier to the model.
Whether showing it improves decisions is a separate question that this makes testable.

## 10. Where this differs from the brief, and why

* **Capability effects are expressed through each item's predicate, not a per-capability declaration.**
  What a capability "may establish" is what an item's predicate lets it establish; a second table of
  per-capability effects would have been a new framework and a second source of truth.
* **The change frontier has three items, not two**, so that looking at the project, making the change and
  verifying it are separately answerable. A list or a read is real progress on the first.
* **A failed execution opens a question** ("does this capability succeed after it failed?"), so that a
  failure changes the frontier rather than only incrementing a counter.
* **A "supporting" class exists** for a successful step in multi-step work that moves no item but tells the
  work something new (a second file read). Without it, ordinary exploration would be miscounted as wrong.
* **A miss begins recovery, not only a failure.** A no-effect decision is a miss as well, which keeps the
  PR38/PR39 meaning of "a wrong valid decision, then recovery" for single-step work.
