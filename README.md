# chip-rs

A small Rust workspace that proves a clean model boundary between the Chip agent runtime and provider execution.

## Workspace

- `crates/fx-core`: provider-neutral model boundary
- `crates/chip-core`: agent runtime
- `crates/chip-cli`: minimal command-line demo

## Usage

```bash
cargo check --workspace
cargo test --workspace
cargo run -p chip-cli -- --test
```

The CLI uses a deterministic in-memory provider and completes a single turn without external credentials or services.
