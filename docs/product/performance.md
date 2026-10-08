# Rust Chip performance baseline

Performance is a first-class product concern. This document is the baseline: how Chip's own cost is
measured, the numbers as first measured, what they show, and what is still unmeasured. **It is a
measurement, not an optimization, and no number here is a budget.** Budgets are set from reality
after a baseline exists; the Target column below is deliberately empty.

**Revision 2 (lazy PAX).** The first finding of the baseline, an unnecessary `pax --version` at the
start of every `chip work`, has been fixed and re-measured; section 1b gives the explicit before and
after. Sections 1 to 2 are the original baseline and are kept as first recorded. Section 3 records
the cause and the outcome of every finding. Raw data for both runs is in
[`performance-baseline.json`](performance-baseline.json) (`runs[0]` is the original, `runs[1]` the
re-run).

## The performance model

The expensive operation is the model. Everything Chip controls around it should be cheap, so that
model latency dominates and Chip's overhead is nearly invisible.

> **Chip must add minimal, bounded overhead around intelligence and execution.**
> No product-path operation should perform unnecessary model calls, filesystem scans, subprocesses,
> serialization, retries, or network requests.

"Bounded" is measured, not asserted: the baseline reports how overhead grows with the length of a
work (section 3). "Unnecessary" is counted: the harness asserts one model call per escalation and one
execution per executed request, and counts the subprocesses a work starts (section 3, finding 1).

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

## 1. Results (original baseline, before the lazy-PAX change)

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

## 1b. After the lazy-PAX change (revision 2)

Same sandbox type and method, one full run (`runs[1]` in the JSON); single figures move by 10-20%
between runs, so read shapes and counts. "Before" is section 1.

| Metric | Before | After |
| --- | --- | --- |
| **`pax --version` processes**, a work that never needs PAX (model stops) | **2** | **0** |
| `pax --version` / git processes, list + two `git status` (no PAX needed) | 2 / 2 | **0** / 2 |
| `pax --version` processes, a work that runs `pax.test` twice | 6 (counted against the old code by the regression test) | **1** (and 2 test runs) |
| `chip work`, process start to first model request (p50 / p95) | 5.5 / 6.4 ms | **2.3 / 2.4 ms** (process floor `--version`: 2.1 ms) |
| `chip work`, process start to exit, one decision | 6.4 ms | 3.1 ms |
| `chip serve`, submit until the model endpoint receives the request | 1.9 ms | 0.45 ms |
| `chip serve`, `scheduling.time_to_first_model_call_ms` | 1.55 ms | 0.17 ms |
| Environment provider prepare | 1.49 ms (one `pax --version`) | 0.004 ms (a filesystem lookup) |
| **L1** 5-capability loop (no PAX needed): Chip overhead p50 / p95 | 1.96 / 2.19 ms | **0.52 / 0.60 ms** (0.45 ms in the earlier partial re-run) |
| L1 full cycle read, write, `pax.test`, evaluate: Chip overhead | 3.5 ms | 1.9 ms |
| L1 full cycle: execution time (cargo through PAX) | 454 ms | 436 ms (within run-to-run noise) |
| L0 work loop overhead, 1 / 50 steps | 12.1 / 1581 us | 10.6 / 1628 us (unchanged, as it should be) |
| Service throughput, instant model, 1 / 16 slots | 213 / 840 works/s | 432 / 1396 works/s |
| Service throughput, 25 ms model, 1 / 16 slots | 12.1 / 158.6 works/s | 12.3 / 175.7 works/s |
| Work start: discover capabilities (new row) | not measured | 25 us, starts no process |
| Validate a `project.read` request (new row) | not measured | 10.8 us |

The L1 hypothesis is supported by the re-run, not assumed. The 5-capability loop dropped by
about 1.4 ms, which is one `pax --version` (1.49 ms measured alone); it had exactly one, at work
start. The full cycle dropped by about 1.6 ms: it had two in the overhead (capability listing and
request validation; the third, in execution, was counted as execution time) and now has one, at
request validation, where PAX is first needed. What remains of L1 overhead without PAX (about
0.5 ms for five capabilities) is 25 us of discovery, about 11 us per request of validation, an
L0-style loop over larger observations, and a residue of roughly 0.3 ms that was not decomposed.
That is under a third of the work's own execution time and four orders of magnitude below a model
call, so it was not pursued.

The counts are the contract; the timings are not. The count assertions live in
`crates/chip-cli/tests/pax_probes.rs` (a counting `pax` at the process boundary under the real `chip`
binary; verified to fail against the old code with `(2, 0)` and `6`) and in
`crates/chip-pax/tests/pax_boundary.rs`, and the benchmark asserts them too.

## 2. What the original numbers said

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

## 3. Findings and what became of them

1. **Two `pax --version` per `chip work`, even when no test is run. Fixed.** Source-level cause,
   traced rather than guessed (before the change):

   ```text
   chip work
   ├─ LocalEnvironmentProvider::prepare            local_environment.rs
   │    └─ PaxExecutor::resolve()  ──► pax --version      (#1: to record the PAX version for the report)
   └─ WorkRuntime::run → Agent::run_work → discover_capabilities   chip-core lib.rs
        └─ CapabilityProvider::availability("pax.test") → PaxExecutor::resolve() ──► pax --version   (#2)
   ```

   and for every `pax.test` request, again: `validate_capability_request` called `availability`
   (#3 per request) and `PaxExecutor::execute` called `resolve()` (#4 per execution). Two requests
   gave 1 + 1 + 2 + 2 = 6. All of them were the same identity probe repeated.

   The fix is inside `chip-pax` and `LocalEnvironment`; `chip-core` is unchanged. `PaxExecutor`
   keeps the verified PAX in a cell that lives exactly as long as the executor (one per work, since
   the environment builds a fresh executor on acquire; clones share it); a success is kept, a failure
   is not. `availability` is now process-free (a `pax` file is where it would be run from);
   `validate_inputs` for `pax.test` is where PAX is first genuinely needed, so it is verified there,
   once, before anything executes; `execute` reuses it. `chip work` and `chip serve` now only
   *locate* PAX at startup, so a missing PAX still stops with exit 3 before a model is asked.
   Behaviour that did change, deliberately: for `chip work` and `chip serve`, a `pax` that is present
   but is not PAX, or is older than 0.3.0, is now reported when `pax.test` is first requested (the
   request is rejected as unavailable and the tests never run) rather than at startup, and the
   report's PAX version is filled in only if the work used PAX. `chip verify` and the proof command
   `--test-pax-work` still verify PAX before asking the model, because PAX is their purpose. The
   guarantee "verified at the moment of use" is now "verified once per executor, at first need".
   Not changed: the remote environment (`chip-remote-env`) runs each request in a separate
   `chip capability-exec` process, so a remote `pax.test` still pays one identity probe in its
   validation process and one in its execution process, and attaching a remote environment still
   asks for `Info`; a cross-process cache would be hidden state and is out of scope.

2. **Overhead per decision grows with the length of the work. Investigated; no change.** What grows,
   from the code and the new measurements:

   | What | Growth | Measured | On the product path? |
   | --- | --- | --- | --- |
   | Escalation context build and measurement (clones the observations and decision strings, renders bytes) | O(history) per model call, so O(n^2) per work | 27 us at 8 x 1 KB observations, 161 us at 50 x 1 KB (about 3.3 us per KiB retained) | yes, every model call |
   | Dedup pass (`omissions`) | O(n^2) per call in general, O(n) when no capability is reusable | n/a | in `chip work` every capability is non-reusable, so it is linear there; it could be cubic over a work only for an embedder with reusable capabilities |
   | Goal predicate (`satisfied_by_trajectory`) | O(observations) per executed observation | 0.13 us per observation (1 us at 8, 128 us at 1000), non-PAX observations | yes, per execution |
   | Event vector pushes | O(1) amortized | 18 events at 1 step, 508 at 50 | yes |

   The L0 growth (6 us per decision at 1 step, 31 us at 50) is accounted for by the first row. At the
   current limits (default 12 turns / 8 executions, ceiling 50 turns) a work spends at most about
   0.2 ms of Chip time in context building, against model calls of hundreds of milliseconds. It is
   not material, and avoiding it would mean caching or incrementally rendering the context, which
   adds state to the loop. **Revisit when** the per-call context build reaches about 1 ms (roughly
   300 KiB of retained history) or 1% of a measured real-model call (needs L2), or when the work
   limits' ceilings go above a few hundred.
3. **Goal evaluation is linear in the trajectory. Recorded; no change.** About 0.13 us per
   observation, evaluated per executed observation, so quadratic over a work, and 1 us in total at
   today's 8 executions. **Revisit when** limits allow around a thousand observations or a predicate
   becomes expensive per observation (the PAX predicate parses a result per PAX observation, which
   the 0.13 us figure does not include).
4. **Capability routing re-enumerates descriptors. Recorded; no change.** `CapabilitySet` asks each
   backend for its descriptors on every availability check, validation and execution; the cost is
   visible as `project.read` validation (5.6 us standalone, 10.8 us as a whole request validation)
   against 0.2 us for the no-op backend. About 25 us at work start for all nine capabilities. Not
   profiled beyond that and not material; no registry or cache was added.
5. **The L1 overhead gap was the PAX probe. Confirmed by the re-run** (section 1b): 1.96 ms to
   0.52 ms, a difference of one probe. A residue of about 0.5 ms for five real capabilities remains,
   partly decomposed in section 1b.

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
* **Remote environments**: a remote `pax.test` still probes PAX in each of its two worker processes (section 3, finding 1).
* **Concurrency ceilings** beyond 16 and on a machine where the client and model are not co-located.
* **Remote and Compute-backed environments.**
* Cost per work in dollars and tokens (needs L2).

## 6. Next

1. **L2 real-model measurement** is the next performance milestone, not another round of
   micro-optimization: it is what says whether any remaining Chip cost matters to a user, and it
   gives the model latency against which the thresholds in section 3 are judged.
2. Keep the counted regressions (subprocesses per work, model calls per decision, executions per
   request) as the contract. No timing budget has been set from these runs, and none is enforced in
   CI.
3. Revisit findings 2 to 4 only at the thresholds stated there.
