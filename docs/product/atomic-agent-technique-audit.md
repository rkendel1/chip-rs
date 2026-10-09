# Atomic Agent technique audit

**Status: research and gap analysis. Docs only: no runtime behavior, no contract and no authority changes.**
Chip remains the sole agent; models are interchangeable reasoning providers behind FX; the runtime owns authority,
execution, evidence and completion. This document asks which of six techniques seen in Atomic Agent would improve
Chip's local-first efficiency, cost, bounded execution and reproducibility, and adopts nothing that has not
earned a measurable hypothesis.

## 0. Source, scope and method

* **Reference:** [AtomicBot-ai/atomic-agent](https://github.com/AtomicBot-ai/atomic-agent) (site: atomicagent.io, which
  was not reachable from the build environment; the repository was). **Pinned revision:**
  `606ef73e94d9372dc7af5957d2c79ad24e3bfffb` (main, 2026-10-08, "feat: preserve full cloud context and migrate provider
  policies (#625)"). MIT licence. TypeScript, about 3,300 files. All "Atomic Agent implements" claims below were read
  in that revision and link to it. Nothing was run, built or benchmarked; this is a code-and-documentation read.
* **Chip:** this repository at the commit that contains this document. "Chip implements" claims were read in the code
  and cite files.
* **What is not claimed.** No Atomic Agent technique is claimed to work well, only to exist. Atomic Agent is a general
  desktop/browser assistant with a different authority model (it lets the model choose and run tools); only
  *techniques* are considered, never its runtime, storage or agent architecture. No code is copied.
* **Vendor behavior** (what Ollama, llama.cpp's server or vLLM accept) is stated from general knowledge and marked
  *unverified here*: the environment has no model runtime and no route to vendor documentation.
* **Measurements:** none exist for any proposal. Every real-model measurement is **blocked** (no model available; see
  [`micro-model-ablation.md`](micro-model-ablation.md)). Mocks, scripted runs and replays are labelled as such wherever
  they appear.

### Invariants every recommendation must keep

1 Chip is the sole agent and orchestration authority. 2 FX stays the provider-neutral model boundary.
3 The Work Contract defines permitted work and acceptance. 4 The Evidence Ledger defines evidence identity,
provenance and freshness. 5 Repository understanding is derived from repository reality. 6 The repair engine
validates preconditions and permissions. 7 PAX supplies observations; it is not a planner. 8 Independent
verification, not model output, establishes completion. 9 Attn is the human control plane, Compute the execution
fabric. 10 No crate, service, store or framework without a documented need. These come from
[`AGENTS.md`](../../AGENTS.md) (including its list of things not to build without justification: model routers,
automatic fallback, retries, repair loops, hidden caches, persistent memory), [`work-contract.md`](work-contract.md),
[`evidence-ledger.md`](evidence-ledger.md), [`blockage-classifier.md`](blockage-classifier.md),
[`micro-step-proposal-gate.md`](micro-step-proposal-gate.md) and [`production-boundary.md`](production-boundary.md).

## 1. Summary matrix

| | Technique | Chip today | Gap | Decision | Tracked as |
| --- | --- | --- | --- | --- | --- |
| A | Constrained decoding for `chip.micro.v1` | Strict validator is authoritative; FX can *request* a JSON object, honored by one of three adapters | A request-time constraint is not representable; the "requested" flag is recorded as if applied | **Experiment** (after a small honesty fix) | RIC-09a |
| B | Stable prefixes and cache reuse | Micro prompt is already stable-first; the work loop puts the stable instructions *after* the variable observations; no cache or timing fields are measured | Layout and cache reuse unmeasured | **Experiment** (measure first) | RIC-09b |
| C | Evidence-driven compression | Bounded outputs, explicit named omissions (`dedup-v1`), a bounded micro snapshot with freshness and provenance ids | No ledger-backed packet; lossy compression conflicts with "nothing summarised" | **Defer** to the Evidence Ledger; one measurable packet experiment | RIC-09c |
| D | Loop detection and bounded execution | Hard limits and repetition *measurement*; the classifier and repair budgets are specified, not built | No repeated-failure stop (HA-09) | **Adopt as design input** to the existing classifier work; no new detector | RIC-04 (existing) |
| E | Trace recording and replay | In-memory work report; micro shadow records; the micro evaluation records raw replies and replays them through the validator and scorer | Work-level replies are not recorded; shadow replies are unredacted | **Defer** (no store); one small redaction gap | RIC-09a note |
| F | Routing and multi-model orchestration | One model per process; escalation specified (BC-R7); micro-model shadow only | No tier, by design for now | **Reject** orchestrator/worker and provider fallback; **defer** routing to RIC-07b | RIC-07 (existing) |

## 2. A. Structured output and constrained decoding

**Atomic Agent.** A GBNF grammar, [`grammars/tool-call.gbnf`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/grammars/tool-call.gbnf#L5), constrains a local
model's whole tool vocabulary: the root is an array so a solo call cannot degrade into a bare object, tool names are an
enumerated alternation, and the runtime can replace rules to splice in the live MCP tool names. It is sent per request
([`llm-link-attempt.ts#L68`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/runtime/llm-link-attempt.ts#L68)); side calls such as compaction send an empty
grammar ([`context-compaction-runner.ts#L131`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/runtime/context-compaction-runner.ts#L131)); for cloud providers
some of its side calls use a `json_schema` response format instead ([`step-contract.ts#L99`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/agent/step/step-contract.ts#L99)). **A lesson in its own comments** ([`tool-call.gbnf#L17`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/grammars/tool-call.gbnf#L17)):
a tool name missing from the grammar *cannot be emitted at all*; a local orchestrator that reasoned "I need
`fusion.delegate`" was pushed into `finish`, "reporting a fan-out that never happened". A constraint that is
incomplete does not fail; it silently changes behavior.

**Chip today.** `micro::validate` ([`micro.rs`](../../crates/chip-cli/src/micro.rs)) is a strict reader and the only authority:
duplicate and unknown fields, closed enums, snapshot and version identity, offered strategies, scope provenance. FX
([`fx-core`](../../crates/fx-core/src/lib.rs)) has no output-format field on `ModelRequest`; `HttpProviderConfig::json_object_output` is a
provider setting that the OpenAI-compatible adapter sends as `response_format` and that the Ollama and Anthropic adapters
**ignore** (documented at [`fx-provider-http/src/lib.rs#L48`](../../crates/fx-provider-http/src/lib.rs), visible in
[`ollama.rs`](../../crates/fx-provider-http/src/ollama.rs), whose wire body has only `temperature` and `num_predict`).
**A finding in Chip's own records:** `micro_eval` run records state `runtime_settings.json_object_output: true`. That is
what was *requested*; on Ollama or Anthropic nothing was applied. The field must be read as "requested" until RIC-09a
makes it truthful. (Not changed here: this PR is docs-only.)

**Gap.** (1) The constraint cannot be expressed per request, and whether it was applied is not recorded. (2) A grammar
for the micro contract could constrain only syntax and closed enums, and, built per request from the snapshot, the
offered strategy ids and fresh evidence ids. It cannot express "this path appears in that evidence record" or "this
snapshot id": those stay validator-only. So constrained output could raise the schema-valid rate, but it cannot make
a reply *correct*.

**Proposed adaptation (RIC-09a).** An optional output-constraint request in FX (`Free`, `JsonObject`, `JsonSchema`; a
provider capability report `supports(constraint)`), default `Free`, so absent support is today's behavior exactly. The
adapters send what they support (vendor support for Ollama's `format`, llama.cpp-server `grammar`/`json_schema`, vLLM
guided decoding: *unverified here*) and the provider response carries `constraint_applied: bool`; the shadow record and
the run record report it, never assume it. No new dependency: these are request fields on the existing HTTP adapters.
The validator stays authoritative whether or not the constraint was applied.

**Compatibility and security.** No authority change: the output is still one `chip.micro.v1` object that cannot carry
code, a capability request or a permission. A schema is public data, not a prompt. The Atomic lesson is the risk:
a closed vocabulary must be complete and must keep `abstained`/`blocked`/`unknown` expressible, or the constraint
*forces an answer*. Measure it as abstention, not as validity.

**Hypothesis.** For a small local model, constraining the reply to the contract raises the schema-valid rate without
lowering classification accuracy or the appropriate-abstention rate, and without raising the false-positive strategy
rate. **Ablation:** the frozen `micro-eval-2` held-out set, same model, same prompt, `Free` versus `JsonObject` versus
`JsonSchema`; report schema-valid rate, rejection codes, accuracy including unknown, false-positive strategy rate,
appropriate and inappropriate abstention, latency (grammar-constrained sampling can be slower), tokens, and
`constraint_applied`. Real model required: **blocked**. **Tests:** adapter unit tests that the constraint appears in the
wire body only when supported; a test that an unsupported constraint is reported as not applied; the existing
validator tests unchanged.

**Decision: Experiment.** Prerequisite (small, not speculative): report `constraint_applied`, so records stop
overstating. **Non-goals:** a model-runtime dependency; constrained decoding of the work-loop decision (a different
contract, its own ticket if ever); treating valid syntax as correct; letting a grammar widen what the model may say.
**Unresolved:** which local runtimes the product will actually ship against; whether a per-request grammar's
construction cost matters on a small CPU-only machine.

## 3. B. Stable prompts and inference efficiency

**Atomic Agent.** A stable prefix (persona, rules, skill catalog, tools, capabilities) is separated from the variable
tail, and the grammar is deliberately outside the cached prefix
([`assembly.md#L6`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/prompt/docs/assembly.md#L6)); catalog or role changes intentionally alter those bytes, loading
a tool does not. Requests pin a server slot and set `cache_prompt` so llama-server picks the slot by prefix similarity
([`llm-link-attempt.ts#L68`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/runtime/llm-link-attempt.ts#L68)); side calls opt out. Its traces record a
`stablePrefixHash`, the tail, token counts, the slot and a `cacheReused` flag per prompt
([`trace-event.ts#L136`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/tracing/trace/trace-event.ts#L136)). Context sizing is derived from memory, because a
unified KV pool shared by parallel slots fails together when its sum is exceeded
([`worker-slots.ts`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/local-llm/server/worker-slots.ts)).

**Chip today.** The micro prompt is already stable-first: a constant system message
([`micro.rs` `SYSTEM_PROMPT`](../../crates/chip-cli/src/micro.rs), digest recorded) then one user message holding the snapshot. The work loop is
not: [`chip-core/src/lib.rs#L1050`](../../crates/chip-core/src/lib.rs) builds the request as the observations (as system
messages, in order) **followed by** the user message, and the goal, completion rule, capability rules and decision
instruction travel in that final message, so the stable instructions sit after the variable, growing observations. Prior
observations remain a byte-stable prefix across turns (they are appended, and `dedup-v1` omissions can shift it), but the
stable instructions cannot be part of a reusable prefix. Nothing records cache reuse: `Usage` carries prompt and
completion tokens only. The Ollama adapter never sends `num_ctx` (audit HA-07), so the server's default window applies,
which also decides whether a long prompt is truncated and whether a changed window evicts the cache.

**Gap.** Layout and cache behavior are unmeasured; the instruction placement is a plausible inefficiency, not a shown one.

**Proposed adaptation (RIC-09b), measurement first.** (1) Record, per request, a digest and byte length of the stable
portion and the variable portion, plus provider-reported prompt-evaluation counts and durations where a provider reports
them (Ollama and llama.cpp report such fields: *unverified here*); unavailable stays unavailable. (2) Only then, an
ablation of the work-loop request layout: stable instructions as a leading system message versus today's tail. This
changes what the model sees *in order*, not what evidence it has; it is a model-visible change and is therefore an
experiment, never a refactor.

**Constraints kept.** No global cache and no hidden state: reuse is whatever the provider server does with identical
prefixes, never a Chip-side cache. No assumption that server state survives between requests; the cost model is "might
hit". The digest binds model id, provider, configuration and the stable bytes, so a change is visible, not served from
a stale entry; nothing is reused across models or contexts because Chip reuses nothing itself. Cache behavior must not
change the evidence available to the model (the ablation checks that the evidence set per turn is identical).

**Hypothesis.** On a local runtime with prefix caching, a stable-first layout lowers prompt-evaluation time per turn
after the first without changing verified completion or regression rates. **Ablation:** fixed task suite (the executed
cases of `micro-eval-2` as tasks via `audit/micro-ablation/run.py`), same model, layout A versus B, N repetitions;
report verified completion and regression with intervals, input and output tokens per success, inference and end-to-end
latency, cache-hit indicators where reported, and an invalidation-correctness check (change the model or the context
size; reuse must not be reported). Real model: **blocked**.

**Decision: Experiment** (measurement hooks first). **Non-goals:** any Chip-side prompt or response cache; slot pinning
as a Chip feature; sending state between requests. **Unresolved:** whether the target runtime exposes reuse at all;
`num_ctx` handling (already HA-07/P1-05).

## 4. C. Evidence-driven context compression

**Atomic Agent.** A result compressor bounds each tool observation by head or tail preservation with a summary cap
([`result-compressor.ts`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/compressor/result-compressor.ts), [README](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/compressor/README.md): tail is kept
on purpose so check verdicts survive), keeps the full output in a transient field for cloud mode, packs history by
budget, with pair eviction and repeat-read substitution in local mode (and none of those in cloud mode, per its [assembly doc](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/prompt/docs/assembly.md)). It is **lossy by design** and relies on the model asking again.

**Chip today.** This is where Chip is already stricter. `chip-project` caps every output at 16 KiB and says when it
truncated ([`MAX_OUTPUT_BYTES`](../../crates/chip-project/src/lib.rs)); the work loop uses `DeduplicatedEscalationContext`
([`software_work.rs#L658`](../../crates/chip-cli/src/software_work.rs)), which leaves out only an observation that its
capability's contract allows to be replaced **and** that a later identical observation makes redundant, and **names every
omission** in the request; "nothing is summarised, truncated or reordered"
([`work.rs`](../../crates/chip-core/src/work.rs)); a context budget refuses a request rather than clipping it; and
`ObservationRepetition` measures new, repeated-identical, changed-reality and retained-repeat bytes. The micro snapshot
([`Snapshot`](../../crates/chip-cli/src/micro.rs)) is already a bounded, provenance-backed evidence packet: ids, freshness flags,
paths, a 400-byte excerpt, a 3,000-byte diagnostic with a truncation flag, stale records withheld but counted.

**Gap.** The packet is derived from the in-memory report because the Evidence Ledger does not exist yet (RIC-03), so there
is no ledger-backed projection, and the diagnostic is a head-bounded text, not an extraction (error codes, failing test
ids). Atomic's lossy summarising is **not** adoptable: it would drop contradictory or stale evidence silently.

**Proposed adaptation (RIC-09c).** After RIC-03, define the packet as a *deterministic projection* of the ledger
(EL-17: a function of contract, ledger and budget, with disclosed omissions): relevant diagnostics, an extracted failure
signature (status, reason, error codes, failing test ids), affected paths with evidence ids, candidate strategies and
budgets; every omitted, stale, truncated or unavailable item named; unknown freshness stays inconclusive and is shown
as such. Before RIC-03, one measurable experiment on the micro fixture only: current head-bounded diagnostic versus the
head-bounded diagnostic plus an extracted signature line, same model.

**Constraints kept.** The ledger stays authoritative and the packet carries references back to it; no second source of
truth; no invented fact (extraction is by pattern from tool output, with the raw text still available by id); no
removal of contradictory evidence (a contradiction is carried, and the classifier treats it as `verification_conflict`).

**Hypothesis.** The extraction-augmented packet keeps classification accuracy and false-positive strategy rate within a
pre-set margin at equal or fewer prompt tokens, and lowers the rate of "diagnostic truncated before the failure" errors.
**Ablation:** `micro-eval-2` held-out, packet P0 versus P1, same model; tokens per decision, accuracy including unknown,
abstention rates, and an *omission-error* count (cases where the discriminating text was cut). Real model: **blocked**.

**Decision: Defer** the ledger-backed packet to RIC-03; **experiment** only the signature-line variant. **Non-goals:**
LLM summarisation of observations; head/tail elision of tool output without a named omission; a repository index as a
second truth. **Unresolved:** per-language extractors beyond Rust/PAX output; the right excerpt sizes (never tuned on
held-out data).

## 5. D. Bounded execution and loop detection

**Atomic Agent.** A per-turn tracker with graduated verdicts (`ok`, `warn`, `critical`, plus a breaker) and several
detectors named in its events: generic repeat (same tool and args hash), no-progress (same result hash), wandering
(many distinct probes of one tool), test repeat, read repeat (an unchanged file re-read) and outcome repeat
([`agent-contract.ts#L738`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/agent/agent-contract.ts#L738), [`loop-detector.ts`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/agent/loop-detector.ts)),
with caps on tracked state, warning de-duplication in buckets, and a veto result that is excluded from the streak. Its
response is to **inject a notice into the model's next prompt** and, at the top, to veto the call.

**Chip today.** The work loop is bounded by turn, execution and context limits; `ObservationRepetition` *measures*
repeats; an identical request is deliberately re-executed (a pass does not cover a later change) and there is a test for
it. The audit found that Chip does not react to a repeating model (HA-09, scripted adversary). The reaction is
specified, not built: BC-R5 `repeated_execution_failure` on the same `(strategy fingerprint, failure signature)` at
least `repeat_threshold` times, with `change_strategy` or `stop` ([`blockage-classifier.md`](blockage-classifier.md), BC-09), and
repair budgets (P1-04 in the [roadmap](coding-agent-production-roadmap.md)), under RIC-04.

**Gap.** Validated, and already tracked: there is no repeated-failure stop. The primitives exist (the invocation key and
`same_reality` that `omissions` uses), so detection needs no new subsystem.

**Proposed adaptation (inside RIC-04, no new ticket).** Key the signal on the *tuple* Atomic's design implies: the same
invocation or strategy against the same **relevant evidence fingerprint**; a repeat after a changed write, a changed
strategy or changed evidence is a new attempt, not a retry. The detector's output is a classifier input, never a policy:
repeated identical failure → BC-R5; a genuine retry with changed evidence is not suppressed; repetition is **not** evidence
of `reasoning_insufficiency` (only BC-R7's explicit bases are). Stop, gather, change strategy or escalate follows the
classified cause. Unlike Atomic's, Chip does not nag the model with a notice: the runtime acts (`change_strategy` / `stop`)
and reports.

**Constraints kept.** No second policy engine; bounded tracked state (as theirs); no hidden retry; limits unchanged.

**Hypothesis.** Stopping or changing strategy at the second identical failure over unchanged relevant evidence cuts
model calls wasted after the first repeat, with no drop in verified completion and no suppressed legitimate retry.
**Ablation:** fixed task suite with repetition-prone tasks (including `n-repeated-*`), baseline (limits only) versus the
classifier path; report verified completion and regression with intervals, model calls and tokens per success, calls
after the first identical failure, **false stops** (a retry with changed evidence that was stopped; must be 0), and the
classification of each stop. The mechanics are testable with scripted models today (the audit scenario
`e-repeat-identical-wrong-write-then-test`); the effect on a real model is **blocked**.

**Decision: Adopt as design input** to RIC-04 (add the "changed relevant evidence is a new attempt" acceptance case).
**Non-goals:** prompt-injected warnings; a wandering detector (Chip's capabilities are few; revisit only if a trace shows
probe sprawl); a separate breaker. **Unresolved:** the right `repeat_threshold` (contract field; unmeasured).

## 6. E. Trace recording and deterministic replay

**Atomic Agent.** An NDJSON trace bus and sink per session with a byte cap
([`trace-sink.ts`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/tracing/trace/trace-sink.ts)); `prompt_captured` stores the stable-prefix hash and the tail so a
prompt can be rebuilt "when combined with a current stablePrefix"; privacy-level redaction of content rows before an issue
report ([`trace-redaction.ts`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/tui/issue-report/trace-redaction.ts)). It also has a **prompt-drift replay** ([`src/replay/`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/replay/README.md)): it rebuilds the current stable prefix and compares its hash with the recorded one to report drift, and an optional inference replay re-runs every recorded prompt against a model client ([`replay-inference.ts`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/replay/replay-inference.ts)). Its own documentation is candid about the limits: replay "does not simulate the external world or guarantee deterministic model output", and "complete secret redaction is not promised" ([`traces.md`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/tracing/docs/traces.md)). (An earlier draft of this audit said no replay existed in the pinned revision; that was wrong, a search that missed `src/replay/`.)

**Chip today.** The work report holds events, decisions, observations, origins and measurements in memory and prints JSON
or text; `--print-reply` writes raw model replies to stderr. The shadow record holds prompt and schema digests, the
bounded raw reply, the validation result and the deterministic outcome beside it. The micro evaluation holds fixture and
held-out hashes, prompt, schema and contract digests, model identity, settings, per-case predictions with raw replies and
rejection reasons, and **replays** recorded replies through the validator and scorer without any model call, labelling
the result `replay`, `new_inference: false`, `evidence_about_a_model: false`
([`micro_eval.rs`](../../crates/chip-cli/src/micro_eval.rs), tested). Replay executes nothing and touches no repository.

**Differences that matter.** Atomic's replay detects *prompt drift* from recorded hashes and can re-run prompts against a live model; Chip's replays *scoring* from recorded replies and calls no model. Chip has no use today for the first (its prompts are constants whose digests are recorded), and Atomic's second form is a fresh inference, which Chip must label as such.

**Gap.** (1) The work loop's model replies are not captured in the report (only the opt-in stderr print), so a work run
cannot be replayed. (2) The shadow record keeps the model's raw reply **unredacted**; a reply can echo repository text
(a secret in a diagnostic). Credentials are never printed (tested), but content echo is not covered. Everything else the
objective lists exists.

**Proposed adaptation.** (2) is a small, validated gap: apply the existing secret-redaction rules to recorded replies
before they enter a report (tracked in RIC-09a as a note, with a test using a diagnostic that contains a token-shaped
string). (1) is **not** adopted: a single exported JSON file the user asks for is an export, not a store, but there is no
requirement today that the existing report and the evaluation records do not meet. No trace database, no byte-capped
sink, no session trace bus.

**Hypothesis / metric.** Replayed scoring reproduces recorded metrics exactly (already tested for the micro evaluation);
extend the same test to any new recorded field. Replayed and scripted results never count as inference measurements
(already enforced by status fields).

**Decision: Defer** the work-level trace; **adopt** the redaction fix. **Non-goals:** a persistent trace store;
tool re-execution during replay; capturing file contents beyond the existing bounds. **Unresolved:** whether the
supervised-use case later needs a work-run export (revisit with P1-02 resume).

## 7. F. Model routing and multi-model orchestration

**Atomic Agent.** Fusion: an orchestrator delegates bounded task waves, with declared deliverables and inputs, to
ephemeral worker turns; workers cannot delegate again; provider pins "must not fall over to a different billing/model
leg" ([`fusion/README.md`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/tools/fusion/README.md)); a fallback chain across providers and a run-mode resolver
for local, cloud and fusion ([`run-mode/README.md`](https://github.com/AtomicBot-ai/atomic-agent/blob/606ef73e94d9372dc7af5957d2c79ad24e3bfffb/src/llm/run-mode/README.md)).

**Chip today.** One provider, model and endpoint per process (HA-10); the model's `escalate` decision is a terminal
`escalated` state; the stronger-model handoff is specified and gated: only `reasoning_insufficiency` (BC-R7), never
missing evidence, an authority gap, ambiguous intent or a verification conflict (BC-14, BC-15; `blockage-classifier.md`),
and is RIC-07b. The micro-model exists only as a shadow classifier and strategy nominator with `authority: none`.

**Gap.** There is no tier, by design. Atomic's orchestrator/worker shape is a *second agent abstraction*: a model
that plans and delegates. That is exactly what Chip's constitution rules out (the runtime is the agent; models do not
delegate to themselves or each other). Its fallback chain is "automatic model fallback" and "hidden retries"
(invariant 7). Two of its constraints are worth keeping verbatim, because Chip's design already shares them: a pin does
not fail over to another model or billing leg, and workers cannot recurse.

**Proposed adaptation.** None to the architecture. Keep: micro-model for bounded classification and nomination only;
stronger model only on explicit `reasoning_insufficiency`; human-addressed stops for the other four causes; the
multi-model approach evaluated against the single-model baseline only in RIC-07c (A/B on the fixed suite).

**Decision: Reject** orchestrator/worker delegation and cross-provider fallback; **defer** routing to RIC-07b and its
evaluation to RIC-07c. **Non-goals:** a second agent, a router, a model that chooses its own model, authority for the
micro-model. **Unresolved:** none that this document can settle; the questions are empirical (does a micro-model's
nomination change larger-model calls?) and need the configuration-C experiment under its own reviewed change.

## 8. Evaluation requirements (all proposals)

Each experiment needs a baseline and an ablation on the fixed suite with independently verified outcomes; report with
denominators and intervals: verified completion and regression rate; input and output tokens per success; larger-model
calls (currently structurally zero: no tier); end-to-end and inference latency; cache hit rate and invalidation
correctness (B); classification and strategy errors (A, C); evidence omission and staleness errors (C); replayed scoring
reproducibility (E). Real-model, mock, scripted and replay results are separate columns; blocked measurements say so. The
harnesses exist ([`micro_eval`](../../crates/chip-cli/benches/micro_eval.rs), [`run.py`](../../audit/micro-ablation/run.py)); real runs are blocked on a model (RIC-07a).

## 9. Roadmap changes

Only validated gaps are tracked, and as one new, small, P2 ticket, **RIC-09 Inference-efficiency experiments**, with
three independently shippable parts (9a constraint capability and truthful reporting plus reply redaction, 9b request
layout and cache measurement, 9c evidence-packet variants). D and F map to existing tickets (RIC-04, RIC-07); E adds
only the redaction note. Nothing here changes a contract, a limit, or an authority.

## 10. Acceptance

| Criterion | Where |
| --- | --- |
| Atomic Agent revision pinned | section 0 |
| Six techniques audited against the code | sections 2 to 7 |
| Decision and rationale each | matrix and per-technique "Decision" |
| Boundaries documented and preserved | section 0 invariants; "Constraints kept" in each section |
| No runtime or authority change | docs only; the diff touches Markdown |
| Roadmap changes correspond to validated gaps | section 9: each gap cites the code that shows it |
| Measurable hypothesis and ablation per implementation | sections 2, 3, 4, 5 (E reuses the existing replay test) |
| Design-docs checks pass | `scripts/check-design-docs.py` |
