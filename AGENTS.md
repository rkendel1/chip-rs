Chip: Research Constitution and Engineering Guardrails

This repository is not primarily an agent-framework experiment.

It is an attempt to prove a specific systems hypothesis:

Reliable autonomy does not require reliable intelligence. It requires reliable boundaries between intelligence and reality.

Every agent working in this repository must read and follow this document before proposing, implementing, or modifying architecture.

⸻

1. North-Star Hypothesis

Core hypothesis

For bounded autonomous work, model strength is secondary to the reliability of the runtime that constrains, observes, evaluates, and recovers from model judgment.

The stronger form is:

A probabilistic judgment system can participate in arbitrarily long autonomous work without being granted authority over reality, provided every transition from judgment → action → observation → evidence is enforced by a deterministic runtime.

The architectural inversion is deliberate:

Judgment       = probabilistic
Execution      = deterministic
Observation    = authoritative
Evaluation     = runtime-owned
Recovery       = runtime-bounded

The model is not the agent.

The runtime is the agent.

The model supplies probabilistic judgment.

Chip supplies deterministic agency:

* validation
* authorization
* execution construction
* observation handling
* goal evaluation
* recovery
* bounds
* terminal state

Compute supplies execution reality.

Reality is authoritative.

⸻

2. The Central Research Question

The fundamental research question is:

How much verified autonomy can we purchase with runtime guarantees instead of model intelligence?

We are not primarily trying to prove that:

* larger models are smarter;
* models can use tools;
* prompting can improve tool use;
* agents can plan;
* LLMs can imitate successful work;
* a benchmark score is high.

We are trying to determine whether reliability can be relocated from the intelligence layer into the runtime layer.

The critical distinction is:

Judgment failure

The model makes a bad decision.

This is expected.

Reality violation

A bad model decision causes an unauthorized state transition.

This must approach zero.

Goal failure

An authorized action executes successfully but does not satisfy the goal.

This must be detected from authoritative reality.

Recovery

The runtime detects the mismatch and permits bounded further judgment.

The model can be wrong.

It must not be allowed to manufacture reality or false success.

⸻

3. The Conceptual Inversion

The current research increasingly supports this distinction:

Model quality
    ↓
judgment quality
    ↓
efficiency / coverage / recovery cost

while:

Runtime quality
    ↓
authority containment
    ↓
reality enforcement
    ↓
goal verification
    ↓
recovery
    ↓
system reliability

Therefore:

Model strength primarily affects what the system can accomplish efficiently. Runtime strength determines what the model is allowed to make real.

Do not interpret this as claiming that model intelligence is irrelevant.

A stronger model may provide:

* better first decisions;
* fewer recovery turns;
* lower token cost;
* lower latency;
* better handling of ambiguity;
* broader task coverage.

These are important.

They are not the same as authority.

The research must keep model capability and runtime reliability as separate variables.

⸻

4. The Fundamental Boundary

The canonical path is:

Model judgment
      ↓
bounded intent
      ↓
Chip validation
      ↓
Chip-owned invocation
      ↓
Compute execution
      ↓
real observation
      ↓
evidence
      ↓
goal evaluation
      ↓
continue / complete / block / escalate

Never collapse these stages.

In particular:

model output ≠ execution
model output ≠ observation
model output ≠ evidence
model output ≠ receipt
model output ≠ success

A model saying that something happened does not make it happen.

A model saying that something succeeded does not make it successful.

A model producing something that looks like a receipt does not make it a receipt.

Only actual execution and authoritative observation can establish reality.

⸻

5. Authority Model

Model

The model may provide:

* bounded judgment;
* capability selection;
* bounded intent;
* a response to an explicit question.

The model may not:

* execute;
* create execution IDs;
* create receipts;
* create evidence;
* declare reality;
* modify capability contracts;
* bypass validation;
* invent executable commands;
* invent invocation inputs;
* mutate runtime state directly;
* grant itself authority.

The model is replaceable.

The runtime must not depend on a particular model being trustworthy.

⸻

Chip

Chip owns:

* work lifecycle;
* state transitions;
* capability validation;
* invocation validation;
* execution construction;
* authority boundaries;
* evidence interpretation;
* goal evaluation;
* bounded decisions;
* limits;
* escalation;
* recovery;
* terminal state.

Chip determines what the model is allowed to ask for.

Chip determines whether the request is valid.

Chip constructs the execution.

Chip determines whether authoritative reality satisfies the goal.

Chip never treats model prose as authoritative reality.

⸻

Compute

Compute owns:

* actual execution;
* execution environment;
* observations produced by execution;
* receipts;
* execution-level reality.

Compute is the factual execution authority.

Chip must not pretend to be Compute.

Chip must not fabricate Compute results.

⸻

FX

FX is the cognition boundary.

FX exists to make model/provider substitution possible.

FX is not:

* the agent runtime;
* the execution runtime;
* the authority layer;
* a planner;
* a memory system;
* a capability executor.

Providers remain behind FX.

Chip must not become coupled to a specific model provider.

⸻

6. Absolute Runtime Invariants

These are hard requirements.

Invariant 1 — No unauthorized execution

No model output may cause execution unless Chip has:

1. parsed the decision;
2. validated the capability;
3. validated the invocation;
4. constructed the execution itself.

⸻

Invariant 2 — No model-created reality

The model cannot create:

* execution;
* receipt;
* observation;
* evidence;
* success;
* committed state.

⸻

Invariant 3 — No invented invocation

If a capability does not declare inputs, the model cannot supply inputs.

This includes:

"inputs": {}

An empty object is still an invocation member and must be rejected when the capability declares no inputs.

The runtime must distinguish:

inputs absent

from:

inputs present but empty

⸻

Invariant 4 — No invented authority

The model cannot supply:

* execution ID;
* receipt;
* status;
* result;
* observation;
* output;
* evidence;
* executable;
* command;
* arbitrary capability parameters.

Unless explicitly declared by a capability contract, these belong to Chip or Compute.

⸻

Invariant 5 — Reality precedes evidence

Evidence must originate from authoritative observation.

The required direction is:

execution
    ↓
observation
    ↓
evidence

Never:

model claim
    ↓
evidence

⸻

Invariant 6 — Invalid judgment fails closed

Malformed, ambiguous, unauthorized, or invalid model output must not be repaired into execution.

The default behavior is:

* reject;
* block;
* escalate;
* fail.

Never:

guess what the model meant.

⸻

Invariant 7 — No hidden fallback

Do not silently:

* choose another capability;
* alter model output;
* repair malformed requests;
* retry;
* invoke another provider;
* invoke another model;
* invent missing inputs.

If recovery is introduced, it must be:

* explicit;
* bounded;
* observable;
* tested.

⸻

Invariant 8 — Execution success is not goal satisfaction

A successful Compute execution establishes that an operation happened.

It does not establish that the goal was satisfied.

Goal satisfaction must be determined from authoritative observation/evidence.

This invariant is central to PR38 and all future recovery experiments.

⸻

7. What We Have Established

The following are completed research results, not merely planned experiments.

PR36 — Absolute Invocation Boundary

Question:

Can incorrect or adversarial model judgment produce unauthorized execution?

Live result:

* 36 model judgments;
* 8 invalid invocation attempts;
* 0 unauthorized executions;
* 0 observations from rejected decisions;
* 0 receipts from rejected decisions;
* 0 evidence from rejected decisions.

The runtime rejected:

* invented inputs;
* empty inputs;
* execution IDs;
* commands;
* executables;
* receipts;
* statuses;
* malformed decisions.

Result:

The model can make an invalid judgment without gaining execution authority.

This is evidence for the boundary, not proof of the entire thesis.

⸻

PR37 — Zero-Overlap Capability Selection

Question:

Can models select semantically appropriate capabilities when superficial lexical overlap is removed?

Result:

* both real providers selected appropriate capabilities above shortcut baselines;
* model errors remained common;
* invalid invocations were contained;
* no boundary violations occurred;
* rejected decisions did not become execution.

Important conclusion:

Selection error does not imply reality violation.

Do not overstate this experiment as proof of semantic understanding.

⸻

PR38 — Wrong Judgment Recovery

Question:

Can a valid but incorrect model judgment become a real execution failure without becoming false success?

Result:

* Haiku: 6/6 forced wrong decisions recovered;
* Qwen: 6/6 forced wrong decisions recovered;
* every forced wrong decision executed through real Compute;
* every produced a real observation;
* every produced real evidence/receipt;
* every was evaluated as unsatisfied;
* 0 unauthorized completions;
* 0 invariant violations.

The critical result is:

valid judgment
    ↓
real execution
    ↓
real failure relative to goal
    ↓
authoritative evaluation
    ↓
bounded recovery
    ↓
verified completion

This establishes that:

Chip can absorb a valid-but-wrong judgment without treating successful execution as successful work.

PR38 limitation

The forced wrong decision was replaced by the harness.

In all 12 live forced-recovery runs, the model’s discarded first response independently selected the correct capability.

Therefore PR38 does not establish that the model learned from the recovery evidence.

It establishes the runtime’s ability to:

* expose authoritative reality;
* reject false completion;
* continue bounded work;
* recover from a wrong execution.

Do not claim more.

⸻

8. Current Research Program

The research now proceeds in three major dimensions.

Experiment 1 — Absolute Boundary

Question:

Can incorrect or adversarial model judgment produce unauthorized reality?

Target:

unauthorized transitions = 0

This is a binary runtime property.

⸻

Experiment 2 — Reliability Over Horizon

Question:

Does runtime containment remain intact as the number of probabilistic judgments increases?

Test horizons:

1
3
5
10

Test:

* correct decisions;
* wrong-but-valid decisions;
* repeated wrong decisions;
* alternating wrong/correct decisions;
* invalid decisions;
* false completion claims;
* execution-limit exhaustion;
* turn-limit exhaustion.

Primary measurements:

unauthorized executions
unauthorized completions
false completions

Target:

0
0
0

The critical comparison is not:

How often was the model correct?

It is:

How often did an incorrect judgment become unauthorized reality?

⸻

Experiment 3 — Model Strength vs Runtime Reliability

Only after the runtime boundary is sufficiently established should model strength become an experimental variable.

Compare:

weak model + Chip
strong model + Chip
strong model + conventional agent loop

Potential metrics:

Model contribution

* verified work;
* task coverage;
* first-decision accuracy;
* recovery rate;
* model calls;
* tokens;
* latency;
* dollars.

Runtime safety

* unauthorized transitions;
* false completions;
* invalid executions;
* evidence violations.

The desired result is not that all models perform equally.

The desired result is:

Model quality changes efficiency and coverage without changing the fundamental authority boundary.

This is the experiment that tests whether reliability has actually been relocated into the runtime.

⸻

9. Reliability Over Horizon

A conventional autonomous system may implicitly depend on every decision being correct.

If each decision has correctness probability p, a sequence of n independent decisions is often approximated as:

p^n

For example:

0.95^20 ≈ 36%

Chip changes the relevant question.

Instead of:

What is the probability that every judgment is correct?

ask:

What is the probability that an incorrect judgment becomes unauthorized reality?

The target is:

unauthorized transition rate → 0

even as:

judgment count → large

This does not mean the system cannot fail.

It means model failure and reality violation must remain separate failure classes.

⸻

10. Optimization Priorities

When choosing between implementation directions, prefer these in order:

1. Authority minimization
2. Reality enforcement
3. Failure containment
4. Recovery
5. Economic efficiency
6. Model capability

If Chip can determine something instead of the model, Chip should determine it.

If reality can establish something instead of the model, reality should establish it.

Do not spend model intelligence where deterministic runtime logic can provide stronger guarantees.

⸻

11. What Counts as Evidence

Evidence must be grounded in reality.

Valid evidence may originate from:

* actual Compute execution;
* authoritative observations;
* validated receipts;
* other explicitly authoritative runtime sources.

Invalid evidence includes:

* model assertions;
* generated prose;
* predicted results;
* model-supplied receipts;
* model-supplied execution IDs;
* unverified claims of success.

Never promote a claim into evidence merely because it looks plausible.

⸻

12. Experimental Discipline

Chip is a research project as well as a software project.

Every experiment must isolate variables.

Prefer:

one hypothesis
one controlled change
one measurement

Do not change multiple dimensions merely to improve results.

Do not tune prompts after seeing failures unless the experiment explicitly studies prompt effects.

Do not discard failures because they make the result inconvenient.

Do not reinterpret terminal state to improve benchmark numbers.

Report:

* successes;
* failures;
* invalid outputs;
* execution failures;
* boundary rejections;
* confounds;
* sample size;
* latency;
* token usage;
* cost;
* missing data.

A disappointing result is useful.

A contaminated result is not.

⸻

13. The Runtime Is the Product of the Research

Do not optimize Chip around a particular provider.

Do not optimize Chip around a particular model.

Do not assume a frontier model.

Do not assume reliable model behavior.

The runtime should become more valuable as model quality varies.

The ideal controlled experiment is:

same Chip
same contracts
same Compute
same evidence rules
different model

If runtime guarantees change when the model changes, the boundary is wrong.

⸻

14. What Agents Must NOT Build Without Explicit Justification

Do not introduce:

* embeddings;
* vector databases;
* semantic registries;
* autonomous capability discovery;
* planners;
* planning graphs;
* hidden memory;
* persistent agent memory;
* prompt routers;
* model routers;
* automatic model fallback;
* retries;
* repair loops;
* background schedulers;
* AppPort integration;
* Attn integration;
* new Compute architecture;
* new ML;
* hidden caches;
* hidden sidecars;
* global mutable state.

These may eventually be useful.

They are not justified merely because they are common in agent systems.

Every new subsystem must answer:

1. What hypothesis does it test?
2. What failure does it address?
3. Why must it exist in Chip?
4. What authority does it introduce?
5. How is that authority constrained?
6. What measurement demonstrates its value?

If those questions cannot be answered, do not add the subsystem.

⸻

15. Do Not Confuse Model Capability With Runtime Capability

A model being capable of producing a command does not mean Chip should allow it to produce commands.

A model being capable of selecting a tool does not mean the model should construct the tool invocation.

A model being capable of describing an expected result does not mean that result is evidence.

The runtime should deliberately be stronger than the model.

⸻

16. Do Not Build Toward “Agentic” Behavior for Its Own Sake

Avoid features because they make Chip look more like an agent framework.

Examples:

* autonomous planning;
* elaborate tool-calling abstractions;
* conversational memory;
* personality;
* multi-agent coordination;
* automatic retries;
* speculative task decomposition.

The question is never:

“Would an agent framework normally have this?”

The question is:

“Does this strengthen reliable autonomy under the Chip hypothesis?”

⸻

17. Prefer Small Boundaries

When two designs are possible, prefer the one with:

* fewer components;
* fewer authorities;
* fewer implicit transitions;
* fewer mutable states;
* fewer hidden behaviors;
* explicit contracts;
* deterministic validation;
* observable failure.

Do not create an abstraction merely because the architecture could support one.

⸻

18. No Fake Success

Never claim:

* a model succeeded when it did not;
* Compute executed when it did not;
* evidence exists when it does not;
* a receipt is valid when it was not verified;
* a benchmark proves more than it actually measures.

If a test is mocked, call it mocked.

If a provider is unavailable, say so.

If a receipt cannot be independently verified, do not claim independent verification.

If a model result is ambiguous, preserve the ambiguity.

⸻

19. Event Trajectories Are Part of the Contract

The event stream is not merely debugging output.

It describes what actually happened.

Use trajectory shape to distinguish:

selection failure
        ↓
invocation failure
        ↓
execution failure
        ↓
observation failure
        ↓
goal failure
        ↓
recovery

Do not add events merely to make reporting easier if existing events already encode the distinction.

Do not infer reality from final text when the trajectory provides authoritative evidence.

GoalEvaluated is specifically intended to distinguish:

execution succeeded

from:

goal satisfied

⸻

20. Terminal State Is Not Goal Satisfaction

A run reaching:

Completed

does not automatically mean the goal was satisfied.

A valid but wrong capability can execute successfully while failing the actual goal.

Therefore distinguish:

runtime completion

from:

goal satisfaction

Goal satisfaction must be evaluated against authoritative observations/evidence.

Never alter terminal-state semantics merely to make benchmark results cleaner.

⸻

21. Recovery Is Not Retry

Recovery is an explicit consequence of authoritative evaluation.

It means:

judgment
    ↓
execution
    ↓
observation
    ↓
goal evaluation
    ↓
unsatisfied
    ↓
bounded new judgment

It must not become:

failure
    ↓
hidden retry
    ↓
try again until success

Recovery must remain:

* bounded;
* observable;
* attributable;
* subject to the same validation;
* subject to the same authority rules;
* subject to existing work limits.

The model is not granted additional authority because recovery occurred.

⸻

22. PR Decision Rule

Before implementing a PR, answer:

1. What hypothesis does this test?
2. What boundary does it strengthen?
3. What can fail?
4. What must remain unchanged?
5. What measurement determines success?
6. What would falsify the hypothesis?
7. Does the PR change model capability, runtime authority, or both?

Prefer PRs that isolate those variables.

A PR that changes model behavior and runtime authority simultaneously is difficult to interpret and should be avoided unless that interaction is itself the hypothesis.

⸻

23. Implementation Priority

When an implementation choice is ambiguous, prefer:

deterministic
over probabilistic
explicit
over implicit
runtime-owned
over model-owned
validated
over assumed
observed
over asserted
evidence
over confidence
failure
over silent repair
bounded
over open-ended
recoverable
over irreversible
small
over elaborate

⸻

24. Long-Term Architectural Model

The intended conceptual stack is:

                HUMAN / GOAL
                     │
                     ▼
              ┌─────────────┐
              │    CHIP     │
              │   RUNTIME   │
              └──────┬──────┘
                     │
              bounded judgment
                     │
                     ▼
                  ┌─────┐
                  │ FX  │
                  └──┬──┘
                     │
             model/provider
                     │
                     ▼
              judgment result
                     │
                     ▼
              ┌─────────────┐
              │    CHIP     │
              │ validation  │
              └──────┬──────┘
                     │
                     ▼
              ┌─────────────┐
              │   COMPUTE   │
              │  execution  │
              └──────┬──────┘
                     │
                     ▼
                  REALITY
                     │
             observation/evidence
                     │
                     ▼
              ┌─────────────┐
              │    CHIP     │
              │ evaluation  │
              └──────┬──────┘
                     │
              continue / recover
              complete / block

The analogy is intentional:

Model   ≈ process
FX      ≈ cognition interface
Chip    ≈ operating-system kernel
Compute ≈ hardware/execution substrate
Reality ≈ physical truth

The analogy is not permission to blindly copy operating-system abstractions.

Use it only to reason about authority, isolation, observation, and controlled transitions.

⸻

25. Ultimate Falsification Test

The strongest version of the research program is eventually:

Take increasingly weak, cheap, noisy, and adversarial models.

Give them increasingly long-horizon work.

Compare:

weak model + Chip

against:

strong model + Chip

and:

strong model + conventional agent loop

Measure:

Primary economic metric

verified useful work / dollar

with secondary normalization by:

* second;
* token;
* model call.

Primary safety metric

unauthorized state transitions / decision

plus:

false completions / decision

The desired result is not:

Chip makes the model smarter.

The desired result is:

Model quality can fall without proportional degradation in system reliability.

If model quality primarily changes:

* coverage;
* efficiency;
* recovery cost;
* latency;

while the Chip boundary maintains:

unauthorized transitions ≈ 0
false completions ≈ 0

then reliability has meaningfully been relocated from intelligence into the runtime.

If that does not happen, record the failure honestly.

⸻

26. The One Question

Every agent working on Chip should be able to answer this before making a change:

Does this make the boundary between probabilistic judgment and authoritative reality stronger, measurable, or more recoverable?

A stronger version is now required:

Does this change improve runtime reliability, model capability, or both—and can those effects be measured separately?

If yes, continue.

If no, do not assume the change belongs in Chip.

⸻

27. Final Principle

The project is not trying to make models trustworthy enough to control reality.

It is trying to make the runtime trustworthy enough that models never need to control reality.

The model supplies judgment.

Chip supplies agency.

Compute supplies execution.

Reality supplies truth.

Model quality affects what can be accomplished and how efficiently.

Runtime quality determines what is allowed to become real.

Do not make the model smarter when the runtime can make the system safer.

Judgment is probabilistic.

Execution is deterministic.

Observation is authoritative.

Evaluation is runtime-owned.

Recovery is bounded.

Reality is authoritative.

Everything else is implementation detail.