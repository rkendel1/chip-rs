# One authoritative FX for Chip

Decision record for "Integrate Rust Chip with authoritative FX" (R6). The authoritative FX is
`rkendel1/fx` (`rust/fx-core`, `rust/fx-single-attempt`, `rust/fx-host`, `host/node`). This repository
no longer carries an FX core of its own.

## Decision

Chip's model port (`chip-inference`) stays Chip-owned. A single adapter crate, `chip-fx`, implements
that port by running exactly one FX attempt per call through the authoritative `fx-core`. Chip does
not call a provider around FX. FX does not depend on Chip.

```
Agent consumer -> Chip (chip-core) -> chip-inference (port) -> chip-fx (adapter) -> fx-core -> provider port
                                                                                      (host: Easy-LLM)
```

## Source-level comparison that drove the decision

| Question | Old `fx-core` here (toy) | Authoritative `fx-core` |
| --- | --- | --- |
| Request model | text prompt, free options | typed `Request`, explicit support flags, refuses tools, images, generation options |
| Delivery certainty | none (error string) | `DefinitelyUnsent < PossiblySent < ConfirmedResponse`, never lowered |
| Outcome | `Result<String,_>` | `ExecutionReport` with outcome, proposal checks, disposition, diagnostics |
| Retries or fallback | none | none (forbidden by contract) |
| Provider I/O | `reqwest` in `fx-provider-http` | host-owned `ProviderPort`; Easy-LLM adapters in Node host |
| Wasm | not buildable | `wasm32` with zero imports, JSON session ABI |
| Conformance | local mock tests | five provider adapters, manifest-checked, run in CI |

Result: the old crate duplicated a weaker contract. It was removed (not kept as a shim) because every
gate below holds.

## Classification of the old `fx-core` contents

| Old item | Disposition |
| --- | --- |
| request/response types | moved to `chip-inference` (Chip-owned port types) |
| error taxonomy | replaced by `FxError::Inference` carrying FX kind, delivery, status, retry hint, partial text |
| `fx-provider-http` | retired: replaced by `chip-fx-native` over FX's native provider; its successor `chip-legacy-http` was then removed |
| any retry, repair, routing code | none existed; none added |

## Adapter contract (`chip-fx`)

* One `ModelRequest` becomes one FX request and one `execute` call.
* `Ok` only for `Completed` with `Disposition::Complete` and no truncation. Everything else
  (length limited, tool proposals, provider failure, invalid completion, cancellation) becomes
  `FxError::Inference` with the delivery certainty preserved.
* Options are explicit: sampling options and a system prompt are not carried (difference D2). The
  policy is named, not defaulted: `Refuse` or `DropUnsupported`.
* Tool proposals from a model are untrusted data. They are never executed by the adapter.
* No retry, no fallback, no routing, no global state.

## Intentional differences from the Zig `fx ask` oracle

Recorded from the real binary in `rkendel1/fx` `docs/rebuild/evidence/zig-oracle.json`.

* D1 Zig repeats a provider request after ambiguous outcomes (429, 500, cut stream). Chip and FX
  never do. A repeat is allowed only when delivery is provably `DefinitelyUnsent`, and chip-rs never
  retries at all.
* D2 Caller-chosen sampling options and system prompt are not carried by the FX path. The Easy-LLM
  OpenAI adapter applies its own fixed temperature of 0.7; that is the adapter's behavior, pinned in
  the fx parity test.
* D3 Zig reports an unknown tool or malformed arguments to the model and continues. Chip fails
  closed without repair.
* D4 Zig sends provider-native tool definitions. Chip uses text decisions (`chip.work-decision.v1`)
  and FX refuses tools.

## Retirement gates

Machine-checked in this repository by `crates/chip-fx/tests/architecture.rs` and
`crates/chip-fx/tests/retirement.rs`, and in `rkendel1/fx` by `docs/rebuild/tools/check_retirement.py`.

| Gate | State |
| --- | --- |
| No second FX core crate in this workspace | met, enforced |
| `chip-core` and `chip-inference` have no FX, `chip-fx`, legacy HTTP or `reqwest` dependency | met, enforced |
| Only `chip-fx` and `chip-wasm-host` reference FX crates | met, enforced |
| No HTTP client crate (`reqwest`, `hyper`, `ureq`) in any workspace crate | met, enforced |
| `chip-legacy-http` removed, not imported, not named by scripts or workflows | met, enforced (`crates/chip-fx/tests/retirement.rs`) |

`chip-legacy-http` and `--executor legacy` were removed; FX is the only executor. The native provider does
not support HTTP proxies or custom CA certificates; see `native-provider.md` (Known limitations).

## Entry points

* In-process Rust: `chip_fx::FxModelProvider` (native), used with `Agent::run_work`.
* Portable: `chip-wasm-host` (`wasm32-unknown-unknown`, no imports) driven by `host/node` in
  `rkendel1/fx` (`createChipRuntime`), with `npm run example:agent` there for an offline multi-step
  run with one authorized tool interaction.

## PR decision rule answers

* Hypothesis: one FX contract serves Chip without behavior loss on the tested path.
* Boundary strengthened: Chip cannot reach a provider except through FX; enforced by tests.
* What can fail: delivery certainty lowered, a retry added, a tool proposal executed, a new legacy use.
  Each has a test that fails (the ratchet was mutation-checked).
* Unchanged: Chip's loop, decision schema, terminal states, no retry doctrine, persistence owner.
* Measurement: integrated tests (chip-fx 22, chip-wasm-host 10, Node end to end 15) plus Zig parity.
* Falsification: a Chip run whose provider sees more than one request per inference, or a report
  whose delivery is lower than FX's.
* Model capability versus runtime authority: the model proposes text decisions; the runtime and the
  host decide what executes.
