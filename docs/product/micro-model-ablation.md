# Micro-model controlled ablation (follow-up to RIC-08)

**Status: the evaluation is built and verified; every real-model measurement is blocked.** No model was available in
the build environment (no local runtime, no model files, no network route to fetch them), so this change contains
**no result about any micro-model** and makes **no promotion decision**. It grants no authority. Read
[`micro-model-shadow.md`](micro-model-shadow.md) first.

## 0. Landing verification, reconciled

Run on commit `6a4e81b` (the RIC-08 landing, tree clean at the start), with PAX 0.4.1 on `PATH`:

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo check --workspace --all-targets` | pass |
| `cargo test --workspace --no-fail-fast` | 151 test binaries `ok`, **0 failures** |
| `scripts/audit-dependencies.sh` | pass |
| `scripts/check-design-docs.py` | pass (71 requirements, 0 problems) |
| `cargo clippy --workspace --all-targets` | 84 diagnostics; **0 in any RIC-08 file**; pre-existing count at `890a832` (before RIC-08) in section 0.1 |

Caveat on that run: two files were edited while it was running (the fixture build script and, after the test binaries
had been compiled, evaluation sources). The authoritative record is therefore the **re-run on the final commit of this
change**, below (section 0.2). Before that, the first run after the landing found one real failure, in the
architecture guard (`micro.rs` named a forbidden crate in a test string); it was fixed in `6a4e81b` and is not
a pre-existing failure. No failure that predates RIC-08 was found.

Evidence: [`audit-evidence/micro-ablation/verification.md`](audit-evidence/micro-ablation/verification.md).

## 1. The evaluation corpus (`micro-eval-2`)

`crates/chip-cli/tests/fixtures/micro/fixture.json`: **50 cases**.

| Tier | Cases | What it is |
| --- | --- | --- |
| `executed_reproduced_and_verified` | 38 | A constructed repository with one known defect. The repository files are **embedded** in the case. Real PAX ran twice with an identical result (status and reason), the labelled correction was applied, and PAX then established `passed`. |
| `unexecuted_synthetic` | 12 | Hand-written diagnostics from the first fixture. **Not ground truth.** Kept as harness fixtures and scored separately; they are never the primary evidence. |

`python3 crates/chip-cli/tests/fixtures/micro/build.py --verify fixture.json` re-materialises every executed case from
its embedded files and re-runs PAX (38 reproduce; 0 problems). That, not a description, is what "independently
reproducible" means here.

Coverage (executed cases): type and trait errors (E0277, E0046, E0308, E0599), unresolved imports and crates (E0432,
E0433), configuration errors (bad manifest value, unparseable manifest, missing library path), assertion failures
(values, strings, ordering, bare `assert!`, `should_panic`), dependency and environment failures (unresolvable
dependency, nonexistent feature, version mismatch, toolchain version, uncreatable build directory in two forms),
**repeated failure after an attempted repair** (a changed write precedes the failure, twice), **stale evidence** and
**missing evidence** (the diagnostic is real; the evidence state is constructed and marked so), **unfamiliar** failures
(custom build-script panic, missing build tool, a link failure, a custom `compile_error!`), **ambiguous** diagnostics
where abstention is correct (a test that exits silently, a test killed by a signal), and **cases where no strategy
applies** (the environment, not the repository, is at fault).

Per case the fixture records the repository state (embedded files and a tree hash, the correction and its hash), the
failure signature (PAX status and reason, rustc error codes, failing tests), the diagnostic evidence, the candidate
strategies, the expected classes and strategies, whether abstention is required, and the verification result.

**Limits.** (a) Labels are by the implementer: every label is `human_review: pending`, and the fixture check counts
independently reviewed cases (currently **0**). The ground truth is *execution*, which is real; the *acceptable
strategy and class lists* are judgement and need a human reviewer. (b) The closed failure-class set has no
"configuration" class; configuration errors are labelled `missing_dependency_or_tooling` or `unknown`, which is a
limitation of the contract the evaluation exposes, not something to hide. (c) Cases are small Rust crates; the
fixture says nothing about large repositories, other languages, or real-world noise.

### Calibration and the frozen held-out set

The initial 22 cases keep their splits. Every case added since is assigned by a rule fixed before any model was run,
using only ids and categories (no result, no label, no model): **within a category, in SHA-256(id) order, each case
goes to the split holding fewer executed cases of that category, ties to held-out.** Every category is present in both
splits. Executed cases: **18 calibration, 20 held-out**. The held-out set (26 cases including its synthetic ones) is
**frozen** in `heldout.freeze.json`: the ids and the SHA-256 of their canonical content, written once by
`micro_eval --freeze`, which refuses to overwrite. Every run record carries `heldout_frozen_intact`; if a held-out case
is changed, added or moved, the run says so and **does not claim to be held-out evidence**. Calibration cases may
inform prompt and threshold choices; held-out cases must not. The prompt and schema hashes are in every record, so
tuning after seeing held-out results is detectable. (The freeze records the commit it was based on; the freezing commit
is the one that contains the file.)

## 2. The three configurations

| | Configuration | State |
| --- | --- | --- |
| A | Deterministic baseline: `chip work` as it is | harness ready; **real run blocked** (no work model) |
| B | A plus the micro-model in shadow | harness ready; **real run blocked** (no work model and no micro-model) |
| C | A plus a micro-model whose eligible nominations influence strategy selection | **not enabled** |

Configuration C needs an explicitly reviewed integration, a Typed Repair Engine to validate preconditions and
authority, and independent verification kept mandatory. None of the first two exists (RIC-04 and the authority
change are separate work), and enabling C inside the shadow-mode PR is exactly what was ruled out.
`run.py --config C` exits 3 with `not_enabled` and the reason. Without C, **this work cannot say whether a micro-model
reduces larger-model calls or improves verified completion**, and nothing here claims either.

`audit/micro-ablation/run.py` runs equivalent tasks (the 36 executed cases that have a correction; the two
environment-fault cases have none) through `chip work` with the same goal text and the same limits in every
configuration, and scores each run with an **independent** verification: a fresh PAX run on the final tree, plus
regression checks (an original file removed, the test surface changed, or fewer tests passing than the reference
corrected tree). A run is a *verified completion* only if Chip said verified **and** the independent run passes **and**
there is no regression; disagreements between Chip and the independent check are counted.

`audit/micro-ablation/compare.py` states what shadow mode must satisfy: identical execution outcomes between A and B,
plus, per B run, that the shadow's own `deterministic` copy equals the process outcome and its `authority` is `none`.
With a deterministic (scripted) work model the A-versus-B check is exact per task. With a real model it is not
exact (the model varies from run to run), so only the per-run consistency is checked, and the A-versus-B difference is
reported with intervals and no significance claim.

## 3. Candidate models

`candidates.json` lists the candidates: Qwen2.5-Coder-1.5B-Instruct, Llama 3.2 1B Instruct, Llama 3.2 3B Instruct,
each with the model names it accepts. `micro_eval --candidate KEY` evaluates a candidate **only if the model actually
configured through `CHIP_MICRO_*` is one of its accepted names**; otherwise the run is blocked with that reason, and no
other model is tried or substituted. The record carries what the operator declares about the artifact (revision,
quantization, inference runtime, context limit, hardware, from `CHIP_MICRO_DECLARE_*`), and `unreported` for anything
not declared, plus sampling (temperature 0, 256 output tokens) and the provider configuration.

All three are **blocked** here: no endpoint is configured and no model can be fetched. The blocked records are in
`audit-evidence/micro-ablation/`. Nothing was substituted and no mock result is reported as a model result.

## 4. Metrics

Reported by configuration and by failure class, with denominators and 95 % Wilson intervals (`k`, `n`, `low`, `high`):

* **Ablation runs (`run.py`):** verified completion; zero-model verified completion; local-only verified completion
  (loopback endpoint, no escalation); regression rate (and among Chip-verified runs); Chip-versus-independent
  disagreements; model calls and tokens per success; total task latency (p50, p95); micro-model latency and tokens
  (B); escalation rate and reasons; cost.
* **Fixture evaluation (`micro_eval`):** schema-valid rate; classification accuracy including `unknown` (an abstention
  predicts `unknown`); false-positive strategy rate; appropriate and inappropriate abstention rates; latency; tokens;
  each overall, per split, per executed-versus-unexecuted tier, and per failure class.

Not conflated: a schema-valid reply is not a correct classification (`schema_valid_rate` and
`classification.accuracy_over_valid_replies` are separate, as is `strategy.false_positive_rate`), and a correct
classification is not a successful repair (no repair is performed by the micro-model; the repair outcome is only
measured in the ablation, by independent verification).

Unavailable measurements, always reported as such, never as zero: **larger-model calls per successful task**
(`chip work` has no larger-model tier today, RIC-07b, so there is nothing to compare); **cost** (no price configured; a
local model's cost is not measured); **micro-model classification accuracy inside ablation runs** (the final failing
state of a run is not labelled; accuracy is measured on the fixture); tokens when a provider reports no usage.

## 5. Reproducibility

Each fixture-evaluation record holds: git commit and working-tree state; fixture version and SHA-256 and the held-out
hash with a frozen-intact flag; prompt, schema and contract hashes; model and provider identity and the declared
candidate metadata; runtime and inference settings; **per-case predictions with the raw reply, the validation or
rejection reason, and the expected labels**; the aggregate metrics and the unavailable-measurement reasons.
Ablation records hold the commit, fixture hash, per-run Chip report fields, the independent verification outcome and
the regression flags.

`micro_eval --replay RECORD` scores recorded replies again. Its record says `status: replay`, `new_inference: false`,
`evidence_about_a_model: false`, and names the record it replays by SHA-256; it reproduces the metrics exactly
(tested). A replay is never presented as a new inference run.

## 6. What was run, and what it shows

| Run | Status | What it shows |
| --- | --- | --- |
| Qwen2.5-Coder-1.5B-Instruct on the fixture | **blocked** | nothing |
| Llama 3.2 1B Instruct on the fixture | **blocked** | nothing |
| Llama 3.2 3B Instruct on the fixture | **blocked** | nothing |
| Substitution check (a different configured model, `--candidate qwen2.5-coder-1.5b-instruct`) | blocked, as designed | the harness refuses a substitute |
| Configuration C | **not enabled** | nothing |
| Configurations A and B, scripted dry run, 36 tasks | `scripted_dry_run` | **the harness**: A and B outcomes identical on all 36 tasks; the shadow was asked on the scripted failures and skipped on the successes; verified completions agree with the independent check. The work model and the micro-model were scripted; this says nothing about any model |

Scripted self-tests (`--self-test oracle|abstain|adversarial`) exercise the scoring: the oracle reaches 1.0 because it
answers from the labels, which validates the arithmetic and nothing else.

## 7. Acceptance

| Criterion | Where |
| --- | --- |
| Full workspace verification reconciled against the evaluated commit | section 0 and the evidence file |
| Fixture distinguishes executed, reviewed cases from unexecuted examples | `execution_status`, `reproducible`, the structural checks, the record's `label_verification` (0 independently reviewed, stated) |
| Calibration and held-out separated; held-out frozen | section 1; test `the_held_out_set_must_match_its_freeze_and_a_changed_case_is_detected` |
| No-provider path unchanged; shadow observational | the RIC-08 tests, unchanged and passing (`tests/micro_shadow.rs`) |
| Ablation compares equivalent inputs and verification rules | `run.py` (same tasks, goal, limits, independent verification); `compare.py` |
| Real-model results distinguished from scripted and replay | `status` and `evidence_about_a_model` in every record |
| No promotion decision from this fixture | section 8 |
| No model authority granted | `authority_granted: none` in every record; configuration C refuses to run |

## 8. Promotion decision

**None.** The micro-model stays in shadow mode. There is no real-model result, the held-out set has 20 executed cases
(an interval from 20 cases is wide: even 20 of 20 has a 95 % lower bound near 0.84), no label is independently
reviewed, and the ablation that would show an effect on larger-model calls or completion cannot run without
configuration C. Promotion needs, at minimum, the gate of `micro-model-shadow.md` section 10 with thresholds fixed in
advance, a reviewed label set, a larger fixture, real-model runs for each candidate on the frozen held-out set, and
the configuration C experiment under a separate reviewed authority change with independent regression testing. The 85 %
local-completion target remains a hypothesis to measure, not a result.

## 9. Run it

```sh
# fixture evaluation (real model): blocked, exit 3, without CHIP_MICRO_*; refuses a non-matching candidate
CHIP_MICRO_PROVIDER=ollama CHIP_MICRO_MODEL=qwen2.5-coder:1.5b-instruct \
  CHIP_MICRO_DECLARE_QUANTIZATION=Q4_K_M CHIP_MICRO_DECLARE_RUNTIME="ollama 0.x" \
  cargo bench -p chip-cli --bench micro_eval -- --candidate qwen2.5-coder-1.5b-instruct
cargo bench -p chip-cli --bench micro_eval -- --replay target/micro-eval/record-<time>.json
# ablation
python3 audit/micro-ablation/run.py --config A --scripted-dry-run --out /tmp/abl      # harness test only
python3 audit/micro-ablation/run.py --config B --scripted-dry-run --out /tmp/abl
python3 audit/micro-ablation/compare.py /tmp/abl/record-A-dry.json /tmp/abl/record-B-dry.json
# real: CHIP_PROVIDER/MODEL/ENDPOINT for the work model, CHIP_MICRO_* for B, then without --scripted-dry-run
python3 crates/chip-cli/tests/fixtures/micro/build.py --verify crates/chip-cli/tests/fixtures/micro/fixture.json
```
