# Chip over the native FX provider

`crates/chip-fx-native` serves Chip's model port (`chip_inference::ModelProvider`) with the authoritative FX
executor and a native Rust provider, `fx-native-provider` from `rkendel1/fx` (see `docs/rebuild/NATIVE_PROVIDER.md`
there). It is the Rust-only route for an embedder that has no Node host.

```
Chip work loop -> NativeModelProvider -> chip_fx::FxModelProvider -> fx_core::execute
                                       -> SingleAttemptCapability<OpenAiCompatible> -> one HTTPS request
```

There is no second executor. Request validation, the single attempt, normalization, cancellation, delivery
certainty and the terminal outcome are the same FX code the Node-backed path uses. One `complete` call is one FX
attempt and at most one HTTP request: no retry, no fallback, no model routing.

## Using it

```rust
let provider = NativeModelProvider::new(NativeSettings {
    base_url: "https://api.openai.com/v1".into(),
    api_key,                      // supplied by the host; this crate reads no environment or file
    options: OptionsPolicy::DropUnsupported,
    budget: Some(Duration::from_secs(120)),
    id_seed: *b"chip-cli",
})?;
let agent = Agent::new(Arc::new(provider)) /* ... */;
```

Settings the native provider refuses (cleartext remote host, credentials in the URL, an empty key) are a
`FxError::Configuration` and no connection is made. The key prints as redacted and is removed from provider
messages. The blocking network call runs on Tokio's blocking pool, so the async work loop is not stalled; the
embedder needs a Tokio runtime.

## What is not covered, and why `chip-legacy-http` stays

The native path carries OpenAI-compatible text conversations only. The legacy executor does more, so the native
`chip` binary has **not** been moved off `chip-legacy-http`:

| Capability | `chip-legacy-http` | `chip-fx-native` |
| --- | --- | --- |
| OpenAI-compatible chat | yes | yes (streaming) |
| Anthropic, Ollama native formats, other adapters | yes | no (Ollama's `/v1` endpoint works) |
| `max_tokens`, `temperature` | sent | refused, or dropped under `OptionsPolicy::DropUnsupported` |
| JSON object output (`json_object_output`) | sent | not carried (FX structured output is unsupported by the port) |
| `enable_thinking` and other server fields | sent | no |
| Anthropic workspace header | yes | no |
| Proxies, custom CA certificates | per `reqwest` | no (bundled public roots, no proxy) |
| Retry or fallback | none | none |
| Delivery certainty on errors | text | structured (`definitely_unsent`, `possibly_sent`, `confirmed_response`) |

Retiring `chip-legacy-http` needs at least: the structured-output and sampling options in FX and the native port,
the providers Chip is actually used with, a live-provider check, and moving each of the 14 consumers in
`legacy-http-consumers.txt`. Until then the ratchet in `crates/chip-fx/tests/retirement.rs` keeps that list from
growing.

## Evidence

`crates/chip-fx-native/tests/native.rs` (9 cases) runs the real Chip work loop, the real FX executor and the
native provider against a loopback mock server: completion, HTTP error with status and `Retry-After`, no retry,
credential redaction, option refusal before any connection, budget timeout, mid-stream cancel with partial
output, a failed attempt ending the work with one provider call, and refused settings. The provider's own
conformance suite and the Node-versus-native parity check live in `rkendel1/fx`. All of it is mocked transport:
nothing here was run against a live provider or over TLS.
