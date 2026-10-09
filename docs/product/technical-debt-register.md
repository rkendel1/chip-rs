# Technical-debt register

Every TODO-like finding in the repository, classified. The goal is **zero unclassified debt**, which
is different from zero debt: the register lists known limitations and what happens to each. Real
work lives in [`coding-agent-production-roadmap.md`](coding-agent-production-roadmap.md); this file
points there rather than duplicating it.

Dated 2026-10-09. Method and counts are in section 1 so the audit can be repeated.

Dispositions: **1 Resolved now** (fixed in the change that introduced this file); **2 Production
defect** (a correctness, safety, reliability or data-integrity problem needing its own change);
**3 Deferred capability** (legitimate work not yet implemented; backlog item named); **4 Intentional
limitation** (explicitly accepted or a documented non-goal).

## 1. What was searched and what it found

Search: `TODO|FIXME|XXX|HACK|todo!(|unimplemented!(|placeholder|for now|temporary|workaround|stub|not
yet implemented|not implemented` over `*.rs`, `*.sh`, `*.yml`, `*.toml`, `*.md`; plus `#[ignore]`;
plus "not yet / future work / out of scope / known limitation / unverified / deferred / not supported /
intentionally" over docs and source comments (about 150 hits, read in context); plus a path and link
check of every repository path named in `README.md`, `AGENTS.md`, `docs/**` and crate READMEs; plus
`cargo clippy --workspace --all-targets` and `cargo check --workspace --all-targets`.

| Marker | Occurrences | Finding |
| --- | --- | --- |
| `TODO` / `FIXME` / `XXX` / `HACK` in code or docs | 2, both test data | `chip-project/tests/navigation.rs:33,255`: a `// TODO: gamma` line in a fixture file that the search test must find. Not debt |
| `todo!(` / `unimplemented!(` | 0 | none |
| `placeholder` | 1 | `chip-reasoning-corpus/src/corpus.rs:105`: documented design (every case carries the same placeholder graph state because the corpus has no architecture). Intentional, experiment crate |
| `stub` | 2 | `chip-cli/src/native.rs:337,347`: a `Stub` `LocalReasoner` inside `#[cfg(test)]`. Test double |
| `temporary` / `mktemp` | many | temp-file implementation details (`chip-project` atomic write, scripts). Not debt |
| `not implemented` | 2 | `README.md` (stale; see D-21, resolved) and `project-observation-boundary.md:184` (Decision Frontier; D-22) |
| `#[ignore]` | 2 | `chip-local-decision-train/tests/training.rs:190` (slow grid search, run in release) and `chip-laya-reasoner/tests/synthetic.rs:266` (a utility that writes a checkpoint). Both documented; neither hides a failing test |

The repository carries its debt as prose limitations, not TODO comments, so the substantive audit is
section 2.

## 2. Register

| ID | Finding | Location | Impact | Disposition | Follow-up |
| --- | --- | --- | --- | --- | --- |
| D-01 | Experiment and proof crates are linked unconditionally into the shipped `chip` binary, and its experiment/proof subcommands ship | `chip-cli/Cargo.toml`, `src/main.rs`; `crates.md` sections 5.1, 8 | The product binary carries unproven code and surface; larger binary; unclear support promise | 3 Deferred: making them opt-in features changes the CLI and needs approval | roadmap P2-D1 |
| D-02 | Duplicate experiments: `chip-wasm-reasoner` vs `chip-wasm-decision` pair; `chip-local-ml` vs `chip-laya-reasoner` | `crates/` | Maintenance cost; no recorded result separates them | 3 Deferred: remove once nobody needs the benchmark commands | roadmap P3-06 |
| D-03 | Product code renders the measurement through a demo module | `software_work.rs`, `verify.rs` -> `work_demo::measurement_json` | A product path depends on proof code; pinned as the only such coupling by `product_path.rs` | 3 Deferred: moving it is a refactor with no behaviour change | roadmap P2-D1 |
| D-04 | `ModelDecisionBoundary` tolerates one code fence around the whole reply | `chip-core/src/model_decision.rs`; `AGENTS.md` Invariant 6 | A narrow exception to "no repair" | 4 Intentional (documented, tested) | none |
| D-05 | A crash between temp file and rename can leave `.chip-write-*.tmp` | `chip-project/src/lib.rs` header | Stray file in the project after a crash | 2 Production defect (minor): detect and report at start | roadmap P0-08 |
| D-06 | Path checks and the operation are not one atomic step (a root rewritten concurrently can race) | `chip-project/src/lib.rs` header | Time-of-check/time-of-use on a project another process mutates | 4 Intentional for a single-owner environment (`Environments` enforces one owner per mutable environment); revisit with isolation | roadmap P0-06 |
| D-07 | Project tooling is not sandboxed: `project.write` + `pax.test` is code execution | `capabilities.md` G6; test `write_plus_test_is_code_execution_through_the_projects_own_tooling` | Hostile project or model-written code runs with the operator's permissions | 2 Production defect for any untrusted/hosted use; accepted for a local trusted developer | roadmap P0-06 (blocked on an isolating provider) |
| D-08 | File contents go to the provider; only `.git` and `.env*` are withheld | `chip-project` path rules; `capabilities.md` G8 | Secrets in other files can reach a hosted model | 2 Production defect before private-repository use | roadmap P0-06 |
| D-09 | Repository content is untrusted input to the model; no document or test addresses prompt injection | docs (absence) | A file can try to steer the model; the decision boundary still refuses invalid decisions but nothing is claimed beyond that | 3 Deferred: needs a threat model | roadmap P0-06 |
| D-10 | Test tampering is invisible to the runtime | `coding-agent-evaluation.md` limit 2 | A green run achieved by weakening tests verifies | 2 Production defect | roadmap P1-V2 |
| D-11 | Completion cannot express feature acceptance | `coding-agent-evaluation.md` limit 1 | Work completes on the first fixed defect | 2 Production defect | roadmap P0-01 |
| D-12 | `inspect` answers are grounded, never verified | `capabilities.md` G1b | Exit 1 by design; no independent predicate exists | 3 Deferred | roadmap P2-C5 |
| D-13 | No repair budget, tier or human channel in `chip work`; an escalated run cannot resume | `coding-agent-evaluation.md` limit 3 | Hard tasks end at the first tier | 3 Deferred | roadmap P1-02, P1-04, P1-07 |
| D-14 | The handoff does not fit the 2,000-byte goal and is test-only | `coding-agent-evaluation.md` limit 4; `tests/coding_agent/packet.rs` | Escalation context is weaker in the product than in the harness | 3 Deferred | roadmap P1-03, P1-08 |
| D-15 | PAX counts are partial when `cargo test` is red | `coding-agent-evaluation.md` limit 7 | Misleading totals; not a verdict error | 3 Deferred (owner: PAX) | roadmap P1-V2 |
| D-16 | Cancellation of running work is advisory; no whole-work wall-clock limit | `service.rs`; `crates.md` section 5.2 | A stuck model call or execution is not interrupted | 2 Production defect (no deadline) / 4 Intentional (advisory cancel, documented) | roadmap P0-04 |
| D-17 | A request for a nonexistent capability exits in the same class as a safety-invariant violation | `capabilities.md` section 2 | Operators cannot tell a bad decision from a broken invariant | 3 Deferred: exit codes are a public interface | roadmap P0-04 |
| D-18 | Events are available only when a work ends; no streaming | `service.rs`; `crates.md` section 8 | Clients cannot observe progress | 3 Deferred | roadmap P1-O3 |
| D-19 | No persistence, authentication or remote exposure in `chip serve` | README; `crates.md` section 10 | Restart forgets all work; local trusted clients only | 4 Intentional (documented non-goal; durable storage is no-go until a requirement exists) | roadmap P3-01, P0-08 |
| D-20 | Tests return early when PAX, Compute, git or a model is absent, so a green run on a bare machine proves less | `crates.md` section 8 ("Skips look like passes") | False confidence | 3 Deferred: print a summary of skipped tests / require them in the release gate | roadmap P2-D5 |
| D-21 | `README.md` said the project-observation boundary "is not implemented" although `project.observe` is | `README.md` | Contradicted `capabilities.md` | **1 Resolved now** | none |
| D-22 | The "Decision Frontier" is described as a direction but implemented nowhere | `project-observation-boundary.md` section 8 | A reader could assume it exists | 3 Deferred (the document says so explicitly) | roadmap P2-04 |
| D-23 | `project-observation-boundary.md` links `observe-benchmark.md`, which exists only on `origin/pax-observe-consumer` | `project-observation-boundary.md` section 10 | Dangling link; the keep/narrow/remove gate for `project.observe` has no result in this branch | **1 Resolved now** (link replaced by an accurate statement); the gate itself is 3 Deferred | roadmap P1-V5 |
| D-24 | No real-model run of `chip work`/`serve`/`verify` is recorded; the opt-in tests are not in CI | `capabilities.md` G10; `crates.md` section 8 | Effectiveness with a real model is unmeasured | 3 Deferred | roadmap P2-05 |
| D-25 | One release target (Linux x86_64), no signing, no evidence of a consumed release | `crates.md` sections 7, 8 | Distribution unverified | 3 Deferred | roadmap P1-O5, P2-D5 |
| D-26 | Escalated works are never evicted from `chip serve`, so that class is unbounded | `service.rs`; README "Retention of finished work" | Memory grows with unresolved escalations | 3 Deferred: needs a resolution mechanism (the service cannot tell resolved from pending) | roadmap P1-O1, P0-03 |
| D-27 | Evidence for an evicted work is gone (410 `work_expired`); evidence does not outlive retention | `service.rs` | A client that polls late loses the explanation | 4 Intentional (documented trade-off of bounded retention) | roadmap P1-V4 |
| D-28 | Adding `max_retained` to the public `Capacity` struct breaks struct-literal construction by embedders (`serve_in`); it broke `chip-cli/benches/baseline.rs`, which `cargo check --workspace` does not compile (only `--all-targets`/`cargo bench` do) | `chip-cli/src/service.rs`; `benches/baseline.rs` | CI's bench step would have failed; an embedder constructing `Capacity { .. }` must add a field or use `..Capacity::default()` | **1 Resolved now** for the in-repo caller (the bench); the compatibility change for external embedders is **4 Intentional** and recorded here | roadmap P2-D5 (API-compat note per release) |
| D-29 | Frozen `decision_state.rs` residue in `chip-core` (nothing in `Agent`/`run_work` reads it) | `chip-core/src/decision_state.rs` | Dead weight kept for experiment goldens | 4 Intentional (pinned by a test that the work loop does not use it) | none |
| D-30 | `chip-compute` is a legacy adapter, not an `EnvironmentProvider`; live runs need a real `compute` | `crates.md` | Frozen integration | 4 Intentional (FROZEN) | none |
| D-31 | Clippy reports about 50 style warnings across the workspace (lints such as `collapsible_if`, `clone` to slice, `assert_eq!` with a literal bool, `is_multiple_of`); none is a correctness lint | listed in section 3 | Noise; no behaviour impact | 3 Deferred: not suppressed and not mass-refactored here; one test-only unused import in `chip-core` and one needless `mut` in the retention bench were fixed | none (mechanical, fix per crate when touched) |
| D-32 | The experiment crate's own limits: Journal payload files carry no checksum; the `durability` crate's lock file and resume-flush behaviour; redb default durability; one machine, synthetic workload, SIGKILL not power loss | `session-store-comparison.md` sections 3, 5, 9 | Only matters if a store is ever adopted | 4 Intentional (experiment findings, recorded) | roadmap P3-01 |

## 3. Clippy warnings, by location (D-31)

Counts from `cargo clippy --workspace --all-targets` on 2026-10-09 (not suppressed; lint names are
clippy's):
`chip-core/src/work.rs` 3x `collapsible_if`; `chip-core/tests/capability_surface.rs` 8x
`unnecessary clone` and 1x `collapsible_if`; `chip-core/src/decision_state.rs` 1x
`wrong_self_convention`; `chip-core/tests/*` 4 more (literal-bool assert, `expect` with a call,
`MutexGuard` held across await in `work_measurement.rs`, a `collapsible_if`); `chip-cli/src/main.rs`
2, `software_work.rs` 1, `work_demo.rs` 2, `horizon.rs` 2, `benches/baseline.rs` 2,
`tests/real_model_work.rs` 1; `chip-compute/tests` 3; `chip-project/src/lib.rs` 1; `chip-remote-env`
2; `chip-pax/tests` 1; `chip-graph/src/analyze.rs` 1; `chip-decision-corpus` 2;
`chip-local-decision-train` 3; `chip-wasm-reasoner/tests` 1; `chip-local-ml/tests` 1;
`chip-session-memory` 8 (experiment). The one with possible runtime meaning is the `MutexGuard` held
across an `.await` in `chip-core/tests/work_measurement.rs` (a test; tracked by D-31 until the test is
next touched).

## 4. What was resolved in the change that introduced this file

* D-21 README contradiction fixed; D-23 dangling link replaced; D-28 baseline bench repaired.
* Stale references corrected: `README.md` named another repository's paths
  (`crates/compute-rust-chip`, `docs/rust-chip.md`) without saying they are not in this one;
  `session-memory-experiment.md` named `crates/feltdb` (FeltDB's repository) as if local;
  the experiment documents did not say they were superseded or label their claims.
* `scripts/audit-dependencies.sh` now also enforces that no product crate reaches an experimental
  storage engine and that nothing depends on `chip-session-memory`.
* No TODO was deleted to reach a count; there were none to delete.

## 5. Verification snapshot (2026-10-09, Linux x86_64, sandbox VM)

See the end of this file once filled by the change author; it is recorded in the pull request
description with the exact commands.
