# Verification record

Environment: Linux, 4 CPUs, 15 GiB, PAX 0.4.1 on `PATH`, no network route to model hosts.

## Run 1 — the RIC-08 landing, commit `6a4e81b` (tree clean at start)

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo check --workspace --all-targets` | pass |
| `cargo clippy --workspace --all-targets` | 84 diagnostics |
| `scripts/audit-dependencies.sh` | pass |
| `scripts/check-design-docs.py` | pass (71 requirements, 0 problems) |
| `cargo test --workspace --no-fail-fast` | 151 test binaries `ok`, 0 failures |

Caveat: files were edited while this run was in progress (fixture build script, later evaluation sources, after the
test binaries had been built), so this run is not by itself authoritative. Run 2 is. An earlier run on the landing
had found one real failure (an architecture guard that scanned a test string in `micro.rs`); it was fixed in `6a4e81b`.

## Run 2 — the evaluated commit, `5c60bfe` (tree clean before and after: `dirty=0`)

This is the commit that contains the fixture, the freeze, the harnesses and the evidence files evaluated by this
change. Only this verification record and the doc text that cites it were added afterwards.

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo check --workspace --all-targets` | pass |
| `cargo clippy --workspace --all-targets` | 84 diagnostics — identical to the 84 at `890a832` (before RIC-08); none in any RIC-08 or ablation file |
| `scripts/audit-dependencies.sh` | pass |
| `scripts/check-design-docs.py` | pass |
| `cargo test --workspace --no-fail-fast` | 150 test binaries `ok`, **1 failure** (below) |

### The one failure

`chip-session-memory` `tests/contract.rs`: `feltdb::compaction_interrupted_at_every_seam_loses_nothing_and_a_rerun_completes`
failed once, under the full-workspace load (61 passed, 1 failed).

* **Not a regression from this change:** `crates/chip-session-memory` has no diff against `890a832` (before RIC-08),
  the crate is a frozen experiment that nothing in the product depends on, and the same test passed in Run 1.
* **Intermittent:** the same test passed 3 of 3 isolated re-runs, and the whole `contract` binary then passed
  (62 of 62) on the same commit.
* **Not root-caused.** It exercises interrupted FeltDB compaction at every seam, which is timing- and
  I/O-sensitive; the panic message was not captured by the filtered log. "Intermittent under load, pre-existing code,
  unexplained" is the accurate status; it is not called a flake on the strength of passing once, and it is not
  attributed to this change. It is recorded here rather than hidden.

## Baseline for the clippy count

`git worktree` at `890a832`, separate target directory: 84 diagnostics. Same at `6a4e81b` and `5c60bfe`.
