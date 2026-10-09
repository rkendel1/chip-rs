#!/bin/sh
# Dependency direction for the Rust stack:
#
#   Rust Chip  ->  Rust FX  ->  generic dependencies
#   Rust Chip  ->  environment contract (chip-core)  <-  whoever provides an environment
#
# Nothing in the agent, its model boundary or its capability transport may depend on a provider of
# environments (Compute or any other). The one allowed edge is chip-cli's demo/benchmark use of the
# `chip-compute` crate; `crates/chip-remote-env/tests/architecture.rs` pins that it stays out of the
# work/serve path.
set -eu
cd "$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)"

fail=0
for crate in chip-core chip-remote-env fx-core fx-provider-http chip-project chip-pax; do
  if cargo tree -p "$crate" -e normal --locked 2>/dev/null | grep -i -E 'compute|feltdb|appport'; then
    echo "FAIL: $crate depends on an environment provider" >&2
    fail=1
  else
    echo "ok   $crate: no compute / feltdb / appport"
  fi
done

if cargo tree -p fx-core -p fx-provider-http -e normal --locked 2>/dev/null | grep -i -E 'zig|npm|node|eve'; then
  echo "FAIL: Rust FX depends on the npm/Zig stack" >&2
  fail=1
else
  echo "ok   Rust FX: independent of the npm stack"
fi

# Experiments and benchmarked storage candidates must stay out of the production path. The session
# memory/store comparison (crates/chip-session-memory) depends on FeltDB, SQLite, redb and the
# `durability` crate; none of them, and not the experiment crate itself, may be reachable from a
# product crate (docs/product/production-boundary.md).
storage='(chip-session-memory|rusqlite|libsqlite3-sys|redb|durability|feltdb) v[0-9]'
for crate in chip-core chip-remote-env fx-core fx-provider-http chip-project chip-pax chip-cli; do
  if cargo tree -p "$crate" -e normal,build --locked 2>/dev/null | grep -E "$storage"; then
    echo "FAIL: $crate reaches an experimental storage engine" >&2
    fail=1
  else
    echo "ok   $crate: no experimental storage engine"
  fi
done
if [ "$(cargo tree --workspace -e normal,build,dev --locked -i chip-session-memory 2>/dev/null | wc -l)" -gt 1 ]; then
  echo "FAIL: a workspace crate depends on chip-session-memory" >&2
  fail=1
else
  echo "ok   nothing in the workspace depends on chip-session-memory"
fi

echo "chip-cli's only edge to an environment provider (demo/benchmark path):"
cargo tree -p chip-cli -e normal --locked --depth 1 | grep -i compute || echo "  (none)"
exit $fail
