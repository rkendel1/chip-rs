PR: test(chip): hostile audit of local-first autonomous coding

Objective

Conduct an adversarial, evidence-driven assessment of Rust Chip from the perspective of an independent developer who wants to use local models as the default coding agent and escalate only when necessary.

Use docs/product/coding-agent-production-roadmap.md as the canonical backlog and acceptance-criteria source. Do not duplicate its capability inventory or introduce a competing roadmap.

The purpose is to establish which autonomous-coding claims are supported by actual production behavior, which remain unproven, and which are blocked by missing capabilities.

Scope

1. Pin and reproduce the baseline

* Record the exact commit SHA, platform, Rust toolchain, PAX version, provider configuration, and required environment.
* Build and test the production workspace using documented commands.
* Confirm the actual chip work, chip serve, and chip verify execution paths.
* Separate environment failures from product failures, with evidence for each classification.
* Record public API and all-target compilation results, including the known risk around Capacity::max_retained and serve_in embedders.
* Do not modify production behavior merely to make the audit pass.

2. Run real-model coding tasks

Use the existing coding-agent evaluation harness where applicable, but explicitly distinguish scripted judgment from real model judgment.

At minimum, evaluate:

* A small, well-specified code change.
* A multi-file feature with independent acceptance criteria.
* A seeded defect requiring a root-cause fix.
* A task where the model’s proposed change does not satisfy the goal.
* A task requiring adaptation after a failed attempt.
* An ambiguous requirement that should not be silently resolved by inventing intent.

Run against at least one real local model if the environment permits. Record the model, runtime, configuration, context limits, hardware, task traces, resource use, and independently determined outcomes.

If a real local model cannot be run, mark the evaluation blocked and state exactly what is needed. Do not substitute scripted judgments and report local-model readiness.

3. Falsify the success and verification guarantees

Attempt to produce false-positive completion through:

* A model claiming success with unmet acceptance criteria.
* A partial or incomplete test run.
* Test deletion, assertion weakening, or ignored tests.
* A failed command that is misinterpreted as success.
* Stale verification evidence after a content-changing write.
* A successful execution whose requested feature remains unimplemented.

For every case, capture the model’s claim, actual repository state, PAX response, Chip’s final state, exit code, and independent acceptance verdict.

Use the existing capability and integrity scenarios where possible. New scenarios must test a distinct failure mode rather than duplicate an existing assertion.

4. Test escalation honestly

Establish which escalation behaviors actually exist in the production path.

Test the existing boundaries around turn limits, execution limits, blocked work, and escalated work. Evaluate the harness’s model ladder separately as an experimental capability.

Identify the minimum missing product mechanisms required for local-first escalation:

* Explicit attempt identity and repair budgets.
* Detection of repeated failure.
* A configurable stronger-model tier, where supported.
* A bounded evidence-based handoff.
* A human-decision channel.
* A stopping policy that does not assume escalation will succeed.

Do not implement the ladder as part of this audit. Do not claim model selection is evidence-driven unless that policy exists and has been tested.

5. Test operational failure boundaries

Use focused tests to examine:

* Cancellation during an in-flight operation.
* Provider and PAX failures.
* Retry and timeout behavior.
* Context-budget exhaustion and omitted observations.
* Completed-work eviction and 410 work_expired.
* Unbounded retention of escalated work.
* Process termination during writes and between execution and result recording.
* Repository changes made outside Chip during a work run.
* Untrusted repository content that attempts to influence the model’s instructions.

Report what is guaranteed, what is merely observed, and what is not tested. Do not add persistence or a sandbox to this PR.

6. Measure the local-first economics

Where real-model instrumentation permits, record task success, regression rate, attempts, provider/model calls, tokens, latency, memory, and escalation outcomes.

Separate local resource use from hosted-provider charges. Identify missing instrumentation explicitly.

Do not infer that a stronger model is more effective from model size or price alone. Do not claim cost savings without comparable task results.

7. Publish the audit report

Create docs/product/hostile-autonomous-agent-audit.md.

Every finding must include:

* A unique finding ID and severity.
* Confirmed, suspected, or untested status.
* Confidence and evidence quality.
* Reproduction steps and affected code path.
* Expected and observed behavior.
* User impact.
* Related roadmap item ID.
* Recommended remediation and measurable closure criteria.

Include an evidence index with commands, environment details, test results, and references to stored traces or benchmark artifacts.

Conclude with separate go, conditional-go, or no-go judgments for:

1. Local-model coding assistance with human supervision.
2. Bounded autonomous coding in disposable repositories.
3. Autonomous coding against important production repositories.
4. Local-first execution with evidence-driven model escalation.
5. Recovery and continuation of interrupted work.

8. Update the canonical roadmap

Update docs/product/coding-agent-production-roadmap.md only where the audit changes the evidence, status, or priority of an existing item.

Reference audit finding IDs from the relevant roadmap entries. Add a new backlog item only if the audit establishes a genuinely distinct gap.

Do not copy the audit findings into a second task list or treat an unimplemented recommendation as an implemented feature.

Constraints

* Audit-first; no broad production refactor.
* No new production dependencies.
* No fake APIs, simulated success, or unverified capability claims.
* No shell, network, Git mutation, or secrets capability added to chip-core.
* No direct production integration of evaluation-only ladder logic.
* No durable session store or process-restart recovery added.
* No claims of local-model readiness without real-model evidence.
* Preserve failed experiments and negative results where they explain a decision.

Acceptance criteria

* [ ]	Baseline commit and environment are pinned.
* [ ]	Existing tests and evaluation harnesses are reused where appropriate.
* [ ]	Real-model testing is completed or explicitly blocked with evidence.
* [ ]	Scripted and real-model results are distinguishable in every report.
* [ ]	Success claims are compared against independent acceptance checks.
* [ ]	Escalation capability is reported according to actual production implementation.
* [ ]	Operational failure boundaries are tested or explicitly marked untested.
* [ ]	Every finding maps to an existing roadmap ID or a justified new item.
* [ ]	The report includes reproducible evidence and readiness judgments.
* [ ]	Roadmap updates reflect findings without duplicating the backlog.
* [ ]	No unrelated production behavior changes are bundled into the audit.

Definition of done

An independent developer can reproduce the material findings and determine, from evidence rather than architectural intent, whether Chip is suitable for their intended level of autonomous coding—and what must change before it can safely do more.
