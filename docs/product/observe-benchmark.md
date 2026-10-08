# `project.observe`: matched baseline-versus-treatment benchmark

Status: **the hypothesis is not supported by this data, and one falsification condition was met.** As a result `project.observe` is **explicit, not default**: it is offered only when `CHIP_ENABLE_PROJECT_OBSERVE=true`. The treatment arm below ran when it was still offered by default; `scripts/bench-observe.py` now opts the treatment arm in explicitly.
The capability is implemented, tested and safe (section 5); that it reduces rediscovery cost is not
shown. Sample sizes are small and the limits in section 6 matter. Reproduce with
`scripts/bench-observe.py`.

## 1. Hypothesis and what would falsify it

> Bounded deterministic project observation reduces model calls, executions and context spent
> rediscovering repository structure, without changing what Chip permits the model to make real.

Falsified (or narrowed) if: model calls do not decrease; context cost does not decrease; the
observation's overhead outweighs savings; verified useful work declines; false conclusions increase;
partial observations confuse; a safety boundary weakens; **the model ignores the observation and
performs the same rediscovery**; or it encourages broader exploration.

## 2. Method

* **Arms.** Treatment is this branch. Baseline is its parent commit (`a6cfb13`): the same code, with
  `project.observe` not offered. The two binaries differ in that and nothing else.
* **Held fixed per (task, model):** the project (a fresh copy), the goal, the model, the provider, the
  runtime and the execution limits (8 executions, 12 turns). One run per cell: the models are
  deterministic at the temperature Chip uses, so repeats would not add information.
* **Tasks (6).** Four inspect questions over a copied slice of this repository's own crates (five
  crates, a real workspace): where a function or enum is declared, which test functions a crate has,
  which source files a crate contains. Two `change` tasks on a purpose-built 14-file crate where one
  small bug in one module fails one test (`B1`: a tax rate, `B2`: a discount threshold), which are
  the only tasks that can produce *verified* useful work. Both fixtures were checked: each bug fails
  exactly one test and the intended fix passes.
* **Models.** Anthropic `claude-haiku-4-5-20251001`; Qwen3.5-35B-A3B-4bit on a local vLLM server;
  `qwen3-coder` on local Ollama.
* **Recorded:** model calls, executions by capability, provider-reported tokens, wall time, Chip's
  `verified`/`goal_satisfied`/useful-work fields, the safety audit, and, labelled as the harness's own
  check and never as Chip's verification, whether the answer or change was right. For inspect tasks Chip
  never reports `verified`, so useful work is 0 in both arms by construction.
* **Pinned PAX:** 0.4.1, tag `v0.4.1`, commit `674f3b3143874d1a692aca103b33f89da31a82ac`.

## 3. Results

### Run 1: description without the scope grammar

| model | arm | valid runs | model calls | executions | tokens | `project.observe` calls | verified | correct (harness) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Haiku | baseline | 6 | 20 | 14 | 32,153 | 0 | 0 | 1 |
| Haiku | treatment | 6 | 17 | 11 | 29,064 | **0** | 0 | 1 |
| Qwen3.5 (vLLM) | baseline | 5 of 6 | 37 | 32 | 60,968 | 0 | 0 | 1 |
| Qwen3.5 (vLLM) | treatment | 6 | 40 | 34 | 65,292 | **0** | 0 | 2 |
| qwen3-coder (Ollama) | baseline | 6 | 40 | 34 | 48,928 | 0 | 0 | 2 |
| qwen3-coder (Ollama) | treatment | 6 | 40 | 34 | 52,043 | **0** | 0 | 2 |

(One Qwen3.5 baseline run did not run: the model server timed out on its first call.)

**Finding from run 1.** The treatment arm made no `project.observe` call in any of 18 runs. Reading the
prompt showed why: **a model is told only a capability's name, its input names and its 160-character
description; an input's own description is not rendered.** The scope grammar lived only in the input
description, so the model had no way to learn it. That is a defect in the capability's contract, not
model behaviour, and run 1 is confounded by it. The description now carries the grammar
(`crate:<pkg> | module:<pkg>/lib::crate[::<mod>] | file:<path>.rs | path:<dir>`), and a test pins that
what the model sees names every accepted form. Run 1 is kept here because it is what first measured.

### Run 2: grammar in the description

| model | arm | valid runs | model calls | executions | tokens | `project.observe` calls | verified | correct (harness) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Haiku | baseline | 6 | 20 | 14 | 32,153 | 0 | 0 | 1 |
| Haiku | treatment | 6 | 28 | 22 | 52,413 | **0** | 0 | 2 |
| qwen3-coder (Ollama) | baseline | 5 of 6 | 38 | 33 | 46,509 | 0 | 0 | 1 |
| qwen3-coder (Ollama) | treatment | 6 | 40 | 34 | 52,481 | **0** | 0 | 2 |
| Qwen3.5 (vLLM) | both | 0 | no data: the vLLM server became unavailable (`Connection refused`) and did not come back | | | | | |

(One Ollama baseline run did not run: the model timed out on its first call, a cold start.)

**With the grammar visible, the models still made no `project.observe` call in any treatment run.**
This held when they were failing: qwen3-coder spent its whole 8-execution budget on `project.list` and
`project.search` on four of six tasks without once choosing the observation. Haiku's runs are
dominated by an unrelated failure: 10 of its 12 runs in run 1 and 9 of 12 in run 2 ended on the
strict-JSON boundary (a reply with prose before the JSON), in both arms.

### Plumbing check (instructed, not part of the comparison)

The goal told the model to use the capability (`scope crate:fx-core`). It works end to end through real
PAX with real models:

| model | arm | calls | executions | tokens | `project.observe` | correct (harness) |
| --- | --- | --- | --- | --- | --- | --- |
| Haiku | baseline | 2 | 1 (one `project.search`) | 2,613 | 0 | yes |
| Haiku | treatment | 3 | 2 (one observe, one read) | 7,597 | 1 | yes |
| qwen3-coder | baseline | 6 | 5 | 7,378 | 0 (the model asked for `project.observe`, which was not declared: rejected, nothing ran) | no |
| qwen3-coder | treatment | 9 | 8 (one observe, seven searches) | 18,062 | 1 | no |

The observation for `crate:fx-core` (34 facts) was about 3 KB as Chip rendered it, against about 14 KB
of PAX JSON for the same scope. For a single-declaration lookup, one `project.search` was cheaper than
observing and then still needing a read to ground the answer.

### Overhead of offering the capability

Where a pair of runs behaved identically, the treatment cost more by the capability's entry in the
prompt: 78 to 170 tokens per run on short runs (the entry is in every request, so it grows with the
number of model calls: 483 to 750 tokens on 8-call runs). One pair was cheaper in the treatment
(336 fewer tokens) because the model's replies differed in length, so this is an estimate, not a constant.

## 4. Against the falsification conditions

| condition | observed |
| --- | --- |
| model ignores the observation and performs the same rediscovery | **met**: 0 calls in the 12 valid treatment runs after the grammar was visible (two models; Qwen3.5 has no run-2 data), and in all 18 before |
| model calls / context do not decrease | no decrease; where behaviour was identical the treatment cost more |
| overhead outweighs savings | there were no savings to weigh it against |
| verified useful work declines | unchanged at 0: no run in either arm verified a change |
| false conclusions increase | not shown; sample too small to say |
| safety boundary weakens | **not met**: every run's audit was clean (section 5) |

## 5. Safety, which held

Across all runs with a valid audit: no unauthorized execution, no unauthorized completion, no false
completion, no path escape, no host-path leak and no out-of-root write. (No malformed observation arose
in these runs, so that case is covered by the deterministic tests, not by this data.) A model that asked for `project.observe` where it was not declared was rejected and nothing
ran. These are the same properties the deterministic tests pin (`crates/chip-pax/tests/pax_observe.rs`,
`crates/chip-cli/tests/capability_scenarios.rs`).

## 6. Limits of this evidence

* **Small and mixed.** One run per cell; three models; six tasks. The models are deterministic at
  Chip's temperature, so repeats would repeat. Two models had infrastructure failures (one server
  went down). This is a weak test, not a strong one.
* **The tasks may be too easy for search.** Two of the four inspect questions are answered by one
  `project.search`, so an observation cannot beat them. A task where `list`/`search`/`read` is
  genuinely expensive (a large unfamiliar workspace, a question that needs the module tree) was not
  tested, and it is where the capability might pay.
* **No change task was solved by any model in either arm,** so verified useful work, the primary
  metric, cannot discriminate here.
* **Haiku's strict-JSON failures** dominate its results and would hide an effect.
* **Inspect tasks cannot be verified by Chip** (by design), so inspect correctness is the harness's own
  check.
* **Not covered:** a model that has been given a reason to prefer observation (that would be a prompt
  or routing study, which this PR deliberately is not).

## 7. Verdict

By the criteria fixed in advance, **the benefit is not demonstrated and the capability should not be
kept as a default-offered capability on this evidence**. The adapter, its boundary and its tests are
sound, and `project.observe` is the right shape if it is wanted; what is missing is a measured reason
to pay its prompt cost on every call. The options are to leave it unoffered by default until a task
class shows a benefit, or to keep it and run a harder matched benchmark first. Neither adds
intelligence to Chip, which is what the brief says to avoid.
