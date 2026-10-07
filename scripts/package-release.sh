#!/bin/sh
# Build the canonical Rust Chip release artifact: one `chip` executable plus a manifest that records
# exactly which Chip and Rust FX it was built from. Chip and Rust FX keep their own versions; the
# manifest does not synchronise them.
#
#   scripts/package-release.sh [output-directory]    (default: dist/)
#
# Writes <out>/chip-<version>-<target>.tar.gz and <out>/chip-<version>-<target>.tar.gz.sha256.
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
out=${1:-$root/dist}
mkdir -p "$out"
out=$(CDPATH='' cd -- "$out" && pwd)
cd "$root"

# --locked: the artifact is built from exactly the dependencies Cargo.lock names.
cargo build --release --locked -p chip-cli

target=$(rustc -vV | sed -n 's/^host: //p')
commit=$(git rev-parse HEAD 2>/dev/null || echo unknown)
dirty=false
if [ "$commit" != unknown ] && [ -n "$(git status --porcelain --untracked-files=no 2>/dev/null)" ]; then dirty=true; fi
stamp=$(git log -1 --format=%ct 2>/dev/null || echo 0)

version=$(cargo metadata --format-version 1 --no-deps --locked | python3 -I -c '
import json, sys
meta = json.load(sys.stdin)
print({p["name"]: p["version"] for p in meta["packages"]}["chip-cli"])')
name="chip-$version-$target"
stage=$(mktemp -d "${TMPDIR:-/tmp}/chip-package.XXXXXX")
trap 'rm -rf "$stage"' EXIT HUP INT TERM
mkdir -p "$stage/$name"

cp "$root/target/release/chip" "$stage/$name/chip"
cp "$root/LICENSE" "$stage/$name/LICENSE"
cp "$root/README.md" "$stage/$name/README.md"

cargo metadata --format-version 1 --no-deps --locked | CHIP_COMMIT="$commit" CHIP_DIRTY="$dirty" CHIP_TARGET="$target" \
  CHIP_RUSTC="$(rustc --version)" python3 -I -c '
import json, os, sys
meta = json.load(sys.stdin)
v = {p["name"]: p["version"] for p in meta["packages"]}
manifest = {
    "format": "chip.release@1",
    "product": "chip",
    "agent": "Rust Chip",
    "version": v["chip-cli"],
    "commit": os.environ["CHIP_COMMIT"],
    "dirty": os.environ["CHIP_DIRTY"] == "true",
    "target": os.environ["CHIP_TARGET"],
    "rustc": os.environ["CHIP_RUSTC"],
    # Two layers that are versioned independently and recorded separately.
    "stack": {
        "rust_chip": {k: v[k] for k in ("chip-cli", "chip-core", "chip-remote-env", "chip-project", "chip-pax")},
        "rust_fx": {k: v[k] for k in ("fx-core", "fx-provider-http")},
    },
    # What a program that hosts or launches this artifact may rely on.
    "interfaces": {
        "executable": "chip",
        "version_output": "chip <version>",
        "service": {"command": "serve", "flags": ["--host", "--port", "--max-concurrent-work", "--max-queued-work"]},
        "capability_worker": {"command": "capability-exec", "arguments": ["--root", "<project directory>"],
                              "request_env": "CHIP_CAPABILITY_REQUEST", "protocol": 1},
        "environment_contract": "chip-core EnvironmentProvider / WorkEnvironment",
    },
    "not": ["npm Chip/Eve", "npm FX/Zig"],
}
json.dump(manifest, open(sys.argv[1], "w"), indent=2, sort_keys=True)
open(sys.argv[1], "a").write("\n")
' "$stage/$name/manifest.json"

# Reproducible archive: sorted names, fixed owner, the commit time as mtime.
(
  cd "$stage"
  tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$stamp" -cf "$name.tar" "$name"
  gzip -n -9 "$name.tar"
)
mv "$stage/$name.tar.gz" "$out/$name.tar.gz"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$out/$name.tar.gz"
