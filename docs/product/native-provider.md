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
    OptionsPolicy::Carry,               // send the token cap and temperature; FX refuses what a port cannot carry
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

## Selecting the executor in `chip`

`chip` can run a model on either executor. The choice is explicit and never a fallback:

```
chip work "<goal>" --provider anthropic --model <model> --executor fx
CHIP_EXECUTOR=fx chip "<prompt>"
```

`--executor legacy|fx`, else `CHIP_EXECUTOR`, else `legacy`. The default stays `legacy` until the gaps below are
closed and a live check is recorded. With `fx`, the same `CHIP_PROVIDER`, `CHIP_MODEL` and `CHIP_ENDPOINT` select the
provider; the endpoint must be the URL the legacy adapters post to (`.../v1/chat/completions` for OpenAI-compatible
servers, `.../v1/messages` for Anthropic), from which the API root is derived. Keys come from `CHIP_API_KEY` if it
is set, otherwise from `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` (see Credentials above). Anything the native path
cannot carry is a configuration error that names it, before any request: the `ollama` provider's own API (its
OpenAI-compatible `/v1` endpoint works), `CHIP_ENABLE_THINKING`, and an endpoint that does not end in the expected
path. `chip work`, `chip serve` and the plain `chip "<prompt>"` turn use the selected executor; the other consumers
of `chip-legacy-http` (listed in `legacy-http-consumers.txt`) have not moved.

## Parity with `chip-legacy-http`

`crates/chip-cli/tests/executor_parity.rs` runs the same configuration and request through both executors against
one loopback server that answers each in the format it asks for. It fails if the FX path drops the sampling options
(checked by mutation). Compared and found **identical**:

* the response text and token usage, for OpenAI-compatible and Anthropic;
* the wire fields `model`, `messages` (including consecutive system messages merged into one), `max_tokens` and
  `temperature` (a configured `0.0` is sent, not dropped), and Anthropic's top-level `system`;
* authentication (`Bearer` key, `x-api-key`, `anthropic-version`) and the Anthropic workspace header;
* the Anthropic fallback cap when nothing sets one (1024), and that Anthropic ignores the JSON-object flag;
* failures for 401, 403, 429, 500 and 503: same coarse class, exactly one request (no retry), and the key never in
  the error.

Differences that are deliberate or known:

| Area | Legacy | FX path |
| --- | --- | --- |
| JSON-object output | `response_format: {"type":"json_object"}` | `response_format: json_schema` with `{"type":"object"}` (what the FX contract expresses; accepted by OpenAI and LM Studio, not verified on OpenRouter models) |
| Transport | one non-streaming call | one streaming call; a stream cut mid-way is a failure with the partial text, never a completion |
| Failure detail | message text | structured: kind, status, retry-after, partial output and delivery certainty (`definitely_unsent`, `possibly_sent`, `confirmed_response`) |
| Endpoint | any URL, posted as given | must end in the provider path; plain `http` only for loopback or without a key |
| Provider APIs | OpenAI-compatible, Anthropic, Ollama native | OpenAI-compatible and Anthropic |
| `CHIP_ENABLE_THINKING` | sent | refused |
| Proxies, custom CA certificates | per `reqwest` | not supported (bundled public roots, no proxy) |

## What still blocks making `fx` the default and retiring `chip-legacy-http`

* **A live check** against Anthropic, LM Studio and OpenRouter. Everything above is mocked transport; TLS has no test.
* The **JSON-object mode** difference on real servers (above).
* LM Studio on a **non-loopback `http`** address (a LAN host) is refused by the native path.
* **Ollama's native API** and `CHIP_ENABLE_THINKING`.
* The remaining consumers of the legacy executor: `work_demo`, `horizon`, `verify`, `pax_work`, `micro`,
  `live_benchmark`, the `--benchmark-live-reasoner` command, and `chip-cli`'s own benches and tests.
* `chip work` and `chip serve` need a real PAX binary, so their end-to-end run through `fx` was not exercised in the
  environment this was built in.

## Evidence

`crates/chip-fx-native/tests/native.rs` (9 cases) and `tests/credentials.rs` (13 cases, with a fake environment and no real keys: resolution, missing keys, LM Studio without authentication, OpenRouter headers and key, redaction, rejected headers) runs the real Chip work loop, the real FX executor and the
native provider against a loopback mock server: completion, HTTP error with status and `Retry-After`, no retry,
credential redaction, option refusal before any connection, budget timeout, mid-stream cancel with partial
output, a failed attempt ending the work with one provider call, and refused settings. The provider's own
conformance suite and the Node-versus-native parity check live in `rkendel1/fx`. All of it is mocked transport:
nothing here was run against a live provider or over TLS.
