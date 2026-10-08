# Rust Chip performance baseline

Performance is a first-class product concern. This document is the baseline: how Chip's own cost is
measured, the numbers as first measured, what they show, and what is still unmeasured. **It is a
measurement, not an optimization, and no number here is a budget.** Budgets are set from reality
after a baseline exists; the Target column below is deliberately empty.

## The performance model

The expensive operation is the model. Everything Chip controls around it should be cheap, so that
model latency dominates and Chip's overhead is nearly invisible.

> **Chip must add minimal, bounded overhead around intelligence and execution.**
> No product-path operation should perform unnecessary model calls, filesystem scans, subprocesses,
> serialization, retries, or network requests.

"Bounded" is measured, not asserted: the baseline reports how overhead grows with the length of a
work (section 4). "Unnecessary" is counted: the harness asserts one model call per escalation and one
execution per executed request, and counts the subprocesses a work starts (section 4, finding 1).

Two things are measured and kept apart, because a faster loop that needs more model turns is not
faster to the user:

* **Speed:** latency, throughput, memory, startup, concurrency.
* **Agency quality:** valid decisions, rejected decisions, unnecessary actions, recovery, escalation,
  model calls per verified goal. The headline is **useful work per model call**.

## Method

`cargo bench -p chip-cli --bench baseline` (`crates/chip-cli/benches/baseline.rs`; not part of the
shipped binary and not run by `cargo test`). `-- --quick` takes fewer samples, `-- --only <section>`
runs one of `parts`, `loop`, `real`, `process`, `serve`, `realmodel`. It prints a table and writes
`target/chip-baseline.json` (`CHIP_BENCH_JSON` overrides the path). The run recorded here is saved as
[`performance-baseline.json`](performance-baseline.json).

| Tier | What it runs | What it isolates |
| --- | --- | --- |
| **L0** runtime overhead | a synthetic in-process model that answers instantly, an in-process do-nothing capability, the real strict decision parser, the real work loop | Chip's own machinery and nothing else |
| **L1** real capability | the same synthetic model; real filesystem, real `git`, real PAX 0.3.0 and cargo, through the real `LocalEnvironment` and the product entry point `run_software_work_with_budget` | Chip plus real execution |
| **L2** real model | the product work runtime against a configured provider | the user-visible end to end |
| process / service | the real `chip` binary: `--version`, `work`, `serve`, with a mock HTTP model endpoint; the service in process with N isolated environments for concurrency | startup, request to work start, memory, throughput |

**Chip overhead of a run = total - model - execution**, where all three are measured by the work loop
itself (`WorkLatency`: observed wall clock, not derived from events). With the synthetic model the
"model" share is a few microseconds and is reported, not hidden.

Asserted on every run (a baseline of a loop that makes extra calls would measure the wrong thing):
model calls = escalations + no retries; executions = executed requests; `chip work` makes exactly one
model request for one decision; the service makes exactly the model calls the script implies.

## Environment of the recorded run

Shared cloud sandbox, 4 vCPU Intel Xeon @ 2.10 GHz, Linux, optimized (`bench`) profile, PAX 0.3.0
built from the commit CI pins. The load generator, the mock model and the service share those four
CPUs. One run, one machine, no isolation from other tenants. Treat the numbers as an order of
magnitude and a shape, not as precise figures; re-measure on the machine you care about before
comparing. **L2 was not run** (no model credentials or endpoint in this environment); no L2 number is
claimed.

## 1. Results

p50 / p95 over n samples. "us" is microseconds.

### Chip's own cost (L0, in process)

| Metric | Target | p50 | p95 | n |
| --- | --- | --- | --- | --- |
| Decision parse, valid request with inputs | - | 1.2 us | 1.9 us | 2000 |
| Decision parse, `complete` | - | 0.4 us | 0.4 us | 2000 |
| Decision parse, rejected (unknown capability / not JSON) | - | 0.6 / 0.2 us | 1.1 / 0.2 us | 2000 |
| Capability validation (no-op backend) | - | 0.2 us | 0.2 us | 2000 |
| Capability dispatch (routing + do-nothing executor) | - | 0.2 us | 0.3 us | 2000 |
| Environment acquire + release (bookkeeping) | - | 0.3 us | 0.3 us | 2000 |
| Observation from a result (2 KB) | - | 0.1 us | 0.3 us | 2000 |
| Observation rendered for the model (2 KB) | - | 5.9 us | 7.8 us | 2000 |
| Evidence record / lookup | - | 0.2 / 0.2 us | 0.2 / 0.2 us | 2000 |
| Event construction (3 representative events; a proxy) | - | 0.1 us | 0.1 us | 2000 |
| Goal evaluation over 1 / 10 / 100 / 1000 observations | - | 0.2 / 1.3 / 12.8 / 127.6 us | 0.2 / 1.3 / 13.4 / 151.8 us | 500 |
| **Work loop overhead, 1 step** (2 decisions, 18 events) | - | 12 us | 24 us | 40 |
| Work loop overhead, 10 steps (108 events) | - | 123 us | 148 us | 40 |
| Work loop overhead, 25 steps (258 events) | - | 502 us | 733 us | 40 |
| Work loop overhead, 50 steps (508 events) | - | 1581 us | 1764 us | 40 |
| **Overhead per decision**: 1 / 5 / 10 / 25 / 50 steps | - | 6 / 9 / 11 / 19 / 31 us | 12 / 11 / 14 / 28 / 35 us | 40 |
| Memory: RSS growth over 200 ten-step works | - | ~0 KiB (RSS 5.6 MiB) | | 1 |

### With real capabilities (L1)

| Metric | Target | p50 | p95 | n |
| --- | --- | --- | --- | --- |
| Validation, `project.read` valid path / rejected `..` | - | 5.6 / 2.0 us | 9.3 / 2.2 us | 2000 |
| Availability check, `project.read` | - | 1.8 us | 1.9 us | 2000 |
| `project.list` (24 entries) | - | 44.6 us | 63.0 us | 2000 |
| Environment acquire + release (local) | - | 2.1 us | 2.2 us | 500 |
| Environment provider prepare (resolves PAX: one `pax --version`) | - | 1.49 ms | 1.97 ms | 30 |
| 5-capability loop (list, search, read, write, git status): Chip overhead | - | 1.96 ms | 2.19 ms | 20 |
| Same loop: execution time | - | 1.49 ms | 2.01 ms | 20 |
| Full cycle (read, write, `pax.test`, evaluate): Chip overhead | - | 3.5 ms | 3.8 ms | 5 |
| Same cycle: execution time (cargo through PAX; first run compiles) | - | 454 ms | 463 ms | 5 |

### Processes and service

| Metric | Target | p50 | p95 | n |
| --- | --- | --- | --- | --- |
| `chip --version`, process start to exit | - | 2.3 ms | 2.8 ms | 50 |
| `chip work`, process start to the first model request | - | 5.5 ms | 6.4 ms | 20 |
| `chip work`, process start to exit (instant model, one decision) | - | 6.4 ms | 7.4 ms | 20 |
| `chip serve`, process start to accepting connections | - | 4.0 ms | | 1 |
| `chip serve`, `POST /v1/work` until 202 | - | 0.29 ms | 0.47 ms | 30 |
| `chip serve`, submit until the model endpoint receives the request | - | 1.9 ms | 2.1 ms | 30 |
| `chip serve`, idle RSS / after 30 works (kept in memory) | - | 6.3 MiB / 8.2 MiB | | 1 |
| **Subprocesses per `chip work`**: `pax` / `git` | - | **2** / 0 (no git capability); 2 / 2 (two git calls) | | 1 |
| Real model latency | provider-dependent | not measured | | |

### Concurrent throughput (service, isolated environments, 3 model calls and 2 real filesystem executions per work, 60 works)

| Concurrent works | Instant model: works/s | submit-to-end p50 | 25 ms model: works/s | submit-to-end p50 |
| --- | --- | --- | --- | --- |
| 1 | 213 | 165 ms | 12.1 | 2.6 s |
| 2 | 444 | 77 ms | 24.0 | 1.3 s |
| 4 | 624 | 56 ms | 47.5 | 0.68 s |
| 8 | 775 | 51 ms | 86.7 | 0.35 s |
| 16 | 840 | 52 ms | 158.6 | 0.20 s |

All 60 submitted at once, so submit-to-end includes queueing. With a 25 ms model, throughput tracks
`concurrency / (3 x 25 ms + overhead)`: it scales with the slots, so the model, not Chip, is the
limit. With an instant model it flattens near 800-850 works/s, which on this machine is the limit of
the shared four CPUs (service, mock model and client together), not a measured Chip ceiling.
RSS grew by 4.4 MiB over the first batch (warm-up), then by 0.05-0.5 MiB per batch of 60 works (negative once): too small to resolve a per-work figure.

## 2. What the numbers say

* **Chip's per-decision cost is microseconds**, four or more orders of magnitude below a model call (hundreds of milliseconds and up).
  A one-step work costs Chip about 12 us; a 50-step work about 1.6 ms.
* **At `chip work` start, Chip is not nearly invisible yet.** Process start to first model request is
  5.5 ms against a 2.3 ms process floor (`--version`), and two `pax --version` subprocesses (about
  1.5 ms each) run on the way. That is the largest Chip-controlled cost on the path, but still small
  next to any real model call (hundreds of ms to seconds).
* **Execution dominates the capability cost**, and cargo through PAX dominates execution (about
  450 ms for a tiny project). Chip's own overhead on the full verify cycle is 3.5 ms.
* **Concurrency scales with isolated environments** up to the machine's limit; with a model-like
  delay it is linear in the slot count.

## 3. Findings (nothing here has been changed)

1. **Every `chip work` runs `pax --version` twice at startup, even when no test is ever run**
   (counted with wrapper scripts: 2 `pax` processes for a work that only stops; 2 `git` processes for
   two Git capabilities, which is as expected). At about 1.5 ms each this is roughly half of the
   5.5 ms start-up. Candidate: resolve PAX once, lazily. This conflicts with the invariant "no
   unnecessary subprocesses", so it is the first optimization target.
2. **Overhead per decision grows with the length of the work** (6 us at 1 step, 31 us at 50), so
   whole-loop overhead is superlinear (1.6 ms at 50 steps, from 12 us at 1). The request sent to the
   model also grows (1.0 KB to 9.0 KB over 50 steps). Not profiled; likely contributors are the
   escalation context being rebuilt from the full history each decision and the goal predicate
   re-scanning the trajectory.
3. **Goal evaluation is linear in the trajectory** (about 0.13 us per observation; 128 us at 1000),
   evaluated each decision, so it is quadratic over a work. Irrelevant at today's limits (12 turns,
   8 executions) and worth knowing if limits rise.
4. **Capability routing re-enumerates descriptors** (`CapabilitySet` calls every backend's
   `capabilities()` for each availability, validation and execution). Read from the code, not
   profiled; the cost is visible as `project.read` validation (5.6 us) against the no-op backend
   (0.2 us), and is small.
5. **The L1 overhead (1.96 ms) is far above L0 (about 12 us per step) and is most likely the PAX
   availability probe**, since one `pax --version` costs 1.3-1.5 ms when measured alone. Not
   profiled; finding 1 would remove it.

## 4. Agency quality

Speed is half the bar. These are already available from the work loop and are what a real-model
(L2) run reports; none can be claimed from a scripted model.

| Metric | Source |
| --- | --- |
| Valid decisions / invalid decisions rejected | `WorkUtilityMeasurement::invalid_decisions`, decision-parse rows |
| Unnecessary actions | `wrong_valid_decisions`, `redundant_selections` |
| Capability selection accuracy | needs ground truth per task; not in this harness |
| Successful goal completion | `verified` (the goal re-evaluated from observations) |
| Recovery rate | `recoveries` / `failed_observations` |
| Escalation rate | `model_escalations` / `turns` |
| Model calls per completed goal; tokens | `model_calls`, `total_tokens` |
| **Useful work per model call** | `verified_outputs / model_calls` |

For the scripted full cycle above it is 1 verified goal over 3 model calls = 0.33. That checks that
the metric is computed; it says nothing about a model. The L2 tier prints these for a real provider:

```text
CHIP_BENCH_REAL_MODEL=1 CHIP_PROVIDER=... CHIP_MODEL=... CHIP_ENDPOINT=... [CHIP_API_KEY=...] \
  cargo bench -p chip-cli --bench baseline -- --only realmodel
```

## 5. Not measured yet

* **Real model latency and L2 agency quality**: needs credentials; the tier is built and reports
  SKIPPED without them.
* **Other machines and other operating systems**; repeated runs for variance (this is one run).
* **Startup of the release artifact** (this uses the bench-profile binary; the release profile is the
  same optimization level but not the packaged file).
* **Memory under long-running service use** (hours, many thousands of works kept in memory), and a
  per-work memory figure: the batches here are too small to resolve it.
* **Event creation inside the loop** (the harness measures a proxy and events per step; instrumenting
  the loop would change the product).
* **Concurrency ceilings** beyond 16 and on a machine where the client and model are not co-located.
* **Remote and Compute-backed environments.**
* Cost per work in dollars and tokens (needs L2).

## 6. Next

1. Resolve PAX once and lazily (finding 1), re-run, compare with `performance-baseline.json`.
2. Set budgets from the re-measured numbers and add a regression check that fails on a counted
   regression (subprocesses per work, model calls per decision) rather than on timings, which are
   noisy on shared machines.
3. Run L2 against the providers in the support matrix and record useful work per model call.
