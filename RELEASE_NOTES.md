# Release notes

## First Rust release

Rust Chip with the authoritative Rust FX is the agent runtime. The Zig agent is not retired by this file; this
is what the Rust release does and does not do.

### Read before you retry

**RETRYING A TURN MAY REPEAT EXTERNAL EFFECTS.** Rust Chip does not guarantee exactly-once side effects, does
not resume an interrupted model response, and does not replay work automatically. After a crash or kill, a new
run starts over and may execute the same capability again. Persistence alone does not make an effect
idempotent. Callers that retry interrupted work must account for effects that may already have happened.

### Changed

* FX is the only inference executor. `--executor legacy` and `CHIP_EXECUTOR=legacy` fail before any request
  with `FX is the only supported executor`; `fx` is still accepted.
* Ollama's own `/api/chat` API is not supported. Use its OpenAI-compatible endpoint with
  `--provider openai-compatible`.

### Not supported (first release)

Permission modes, provider-native and parallel tool calls, silent tool continuation, pause/continue at the
step limit, image input, subagents / MCP / skills / memory / web tools, interactive approval, graceful
cancellation between a tool and the next request, publish-failure handling after the work ran, and any
idempotence guard. Each is refused or fails closed with an explicit message; none reports a completion. See
`docs/product/release-limitations.md`.

### Known limitations carried into this release

* The native provider does not support HTTP proxies or custom CA certificates (issue #35).
* No live OpenRouter evidence (waived for this migration; not a claim that it works).
* Anthropic and LM Studio live runs were reported clean by the owner; their raw output is not in the repository.
