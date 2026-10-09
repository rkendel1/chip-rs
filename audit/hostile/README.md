# Hostile-audit harness

Reproduces `docs/product/hostile-autonomous-agent-audit.md`. **Audit infrastructure only**: not a workspace
member, no Rust, standard-library Python 3, no production dependency, no production behavior changed.

The "model" is `mockmodel.py`, a scripted OpenAI-compatible server that plays an adversary (confidently wrong,
obedient to injected text, repetitive, malformed). **It is not a model. Nothing here measures any real model.**
The independent acceptance verdict comes from `fixture.accept`, which re-runs the pristine visible tests plus
hidden tests on a copy with plain `cargo test`, never through Chip or the model.

```sh
cargo build --release --locked -p chip-cli            # and PAX 0.4.1 on PATH
export CHIP_BIN=$PWD/target/release/chip PAX_BIN=$(command -v pax) AUDIT_ROOT=/tmp/aud AUDIT_OUT=/tmp/aud/out
cd audit/hostile
python3 verification.py                 # false-success attempts (V-series)
python3 operations.py o1 o2 o3 o4 o5 o6 # provider/PAX failures, context, hostile repo, injection, secrets, path fuzz
python3 lifecycle.py                    # cancel, escalated retention, restart, duplicates, external edit, two processes, kills
python3 escalation.py                   # production escalation boundaries, instrumentation inventory
python3 flood.py; python3 hung_tooling.py   # silent output truncation; hung real PAX (about 5 minutes)
```

A run needs about 5 GB of disk for the shared cargo target and writes under `AUDIT_ROOT`. `o2` includes a
300-second hang case. A real-model run needs `runner.base_env` pointed at a real endpoint and the scripted
`script` replaced by no script at all; it has not been done (see the report, section 2).
