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
// The user chose the provider, endpoint and model explicitly. Nothing here selects or falls back.
let selection = NativeSelection::new(ProviderKind::Anthropic);        // ANTHROPIC_API_KEY
let settings = NativeSettings::resolve(
    &selection,
    &EnvCredentials::process(),         // Chip's host layer reads the environment, once
    OptionsPolicy::DropUnsupported,
    *b"chip-cli",
)?;                                      // missing key: a Configuration error, no request made
let provider = NativeModelProvider::new(settings)?;
let agent = Agent::new(Arc::new(provider)) /* ... */;
```

## Credentials: Chip acquires, FX consumes

```
CredentialSource  ->  resolved credential  ->  NativeSettings (FX provider configuration)
```

Chip's host layer (`credentials.rs` in this crate) is the only place that reads the environment; a test enforces it.
The native FX provider receives the one resolved key, or none, through its configuration and never inspects the
environment or a credential file.

| Provider | Variable | Notes |
| --- | --- | --- |
| Anthropic | `ANTHROPIC_API_KEY` | always required |
| OpenAI-compatible | `OPENAI_API_KEY` | required unless the endpoint is a plain-`http` loopback address |
| OpenRouter | a variable the user names (`key_env`, for example `OPENROUTER_API_KEY`) | uses the OpenAI-compatible provider; attribution headers via `headers` |
| LM Studio | none | `http://localhost:1235/v1`: no key needed, and one is used if the variable is set |

Rules: a named variable that is unset is an error, never a silent fall back to another variable (an
`OPENAI_API_KEY` is not sent to OpenRouter because `OPENROUTER_API_KEY` is missing); blank values count as unset and
values are trimmed; a variable name must look like one; the error names the variable to set and says no request was
made. The key prints as `[redacted]` in `Debug`, is removed from provider messages, and is never part of an FX
request, so it cannot reach a session log. Headers the adapter owns (authorization, `x-api-key`, content type,
host, `anthropic-version`) are rejected when the provider is built.

An interactive prompt or the Easy-LLM vault can implement `CredentialSource` later without touching provider
execution. Neither exists today, deliberately: the vault would couple Rust Chip to its file format, encryption and
master-password lifecycle. The Node-backed path keeps using the vault and Easy-LLM's router unchanged.

The blocking network call runs on Tokio's blocking pool, so the async work loop is not stalled; the embedder needs a
Tokio runtime.

## What is not covered, and why `chip-legacy-http` stays

The native path carries OpenAI-compatible text conversations only. The legacy executor does more, so the native
`chip` binary has **not** been moved off `chip-legacy-http`:

| Capability | `chip-legacy-http` | `chip-fx-native` |
| --- | --- | --- |
| OpenAI-compatible chat (OpenAI, OpenRouter, LM Studio) | yes | yes (streaming) |
| Anthropic | yes | yes (streaming) |
| Ollama native format, other adapters | yes | no (Ollama's `/v1` endpoint works) |
| `max_tokens`, `temperature` | sent | refused, or dropped under `OptionsPolicy::DropUnsupported` |
| JSON object output (`json_object_output`) | sent | not carried (FX structured output is unsupported by the port) |
| `enable_thinking` and other server fields | sent | no |
| Anthropic workspace header | yes | yes, as an extra header |
| Proxies, custom CA certificates | per `reqwest` | no (bundled public roots, no proxy) |
| Retry or fallback | none | none |
| Delivery certainty on errors | text | structured (`definitely_unsent`, `possibly_sent`, `confirmed_response`) |

Retiring `chip-legacy-http` needs at least: the structured-output and sampling options in FX and the native port,
the providers Chip is actually used with, a live-provider check, and moving each of the 14 consumers in
`legacy-http-consumers.txt`. Until then the ratchet in `crates/chip-fx/tests/retirement.rs` keeps that list from
growing.

## Evidence

`crates/chip-fx-native/tests/native.rs` (9 cases) and `tests/credentials.rs` (13 cases, with a fake environment and no real keys: resolution, missing keys, LM Studio without authentication, OpenRouter headers and key, redaction, rejected headers) runs the real Chip work loop, the real FX executor and the
native provider against a loopback mock server: completion, HTTP error with status and `Retry-After`, no retry,
credential redaction, option refusal before any connection, budget timeout, mid-stream cancel with partial
output, a failed attempt ending the work with one provider call, and refused settings. The provider's own
conformance suite and the Node-versus-native parity check live in `rkendel1/fx`. All of it is mocked transport:
nothing here was run against a live provider or over TLS.
