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

`--executor legacy|fx`, else `CHIP_EXECUTOR`, else **`fx`**. `legacy` is never chosen implicitly and nothing falls back
to it: it is an explicit, temporary escape hatch for OpenAI-compatible and Anthropic endpoints and goes away with
`chip-legacy-http`. Ollama is reached through its OpenAI-compatible `/v1` endpoint on either executor. With `fx`, the same `CHIP_PROVIDER`, `CHIP_MODEL` and `CHIP_ENDPOINT` select the
provider; the endpoint must be the URL the legacy adapters post to (`.../v1/chat/completions` for OpenAI-compatible
servers, `.../v1/messages` for Anthropic), from which the API root is derived. Keys come from `CHIP_API_KEY` if it
is set, otherwise from `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` (see Credentials above). Anything the native path
cannot carry is a configuration error that names it, before any request: `CHIP_ENABLE_THINKING` on the Anthropic provider (which has no such field; the legacy adapter ignored it there), and an endpoint that does not end in the expected
path. `chip work`, `chip serve` and the plain `chip "<prompt>"` turn use the selected executor (the default is `fx`); every other model-calling command
(`verify`, `work_demo`, `horizon`, `pax_work`, the micro shadow, `--benchmark-live-reasoner`, the `micro_eval` bench) builds its
provider through the same selection (`CHIP_EXECUTOR`, default `fx`). Only `provider_selection.rs` constructs the legacy
provider, and `crates/chip-fx/tests/retirement.rs` fails if anything else does.

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
| Provider APIs | OpenAI-compatible, Anthropic | OpenAI-compatible and Anthropic (Ollama's own API is unsupported on both; see Ollama below) |
| `CHIP_ENABLE_THINKING` | sent as top-level `chat_template_kwargs.enable_thinking` (OpenAI-compatible only; ignored on Anthropic) | the same field on OpenAI-compatible endpoints, both `true` and `false`; refused by name on Anthropic |
| Plain `http` to a LAN host | any URL | refused unless `CHIP_ALLOW_INSECURE_LAN_HTTP=1` and the host is a private IP literal (RFC 1918 or IPv6 unique-local); keyless OpenAI-compatible only, never with a key from any source, no extra headers, no hostnames, redirects not followed |
| Proxies, custom CA certificates | per `reqwest` | not supported (bundled public roots, no proxy) |

## What remains before `chip-legacy-http` can be retired

`fx` became the default with these still open (decision recorded in the default-flip PR):

* **Live checks.** Anthropic and LM Studio were run by the owner and reported clean; the raw output and audit have not been added to the repository. **OpenRouter has not been run.** Everything else above is mocked transport; TLS has no test.
* The **JSON-object mode** difference on real servers (above).
* The legacy executor is now only an explicit option. What still names `chip-legacy-http` is listed in
  `legacy-http-consumers.txt` (the shared config type, the explicit `legacy` constructor, one unit-test module and the
  tests). `ProviderConfig` has moved to `chip-inference`, and Ollama's own API is dropped (below). Deleting the crate means
  removing `--executor legacy` and porting or deleting the tests that still build a legacy provider.
* The horizon live run needs real Compute before it asks a model; its model request was not exercised end to end here,
  only its provider selection.

## Evidence

`crates/chip-fx-native/tests/native.rs` (9 cases) and `tests/credentials.rs` (13 cases, with a fake environment and no real keys: resolution, missing keys, LM Studio without authentication, OpenRouter headers and key, redaction, rejected headers) runs the real Chip work loop, the real FX executor and the
native provider against a loopback mock server: completion, HTTP error with status and `Retry-After`, no retry,
credential redaction, option refusal before any connection, budget timeout, mid-stream cancel with partial
output, a failed attempt ending the work with one provider call, and refused settings. The provider's own
conformance suite and the Node-versus-native parity check live in `rkendel1/fx`. All of it is mocked transport:
nothing here was run against a live provider or over TLS.

## Ollama

Decision (2026-10-16, repository owner's default preference): **Ollama's own `/api/chat` API is not a supported provider** and no native implementation is built. Ollama's OpenAI-compatible endpoint is the supported path:

```
chip work --provider openai-compatible --endpoint http://127.0.0.1:11434/v1/chat/completions --model <model> "<goal>"
```

`CHIP_PROVIDER=ollama` (and `--provider ollama`, `CHIP_MICRO_PROVIDER=ollama`) now fails before any request with a configuration error that names this replacement; nothing falls back. `CHIP_OLLAMA_ENDPOINT` is no longer read. Supporting `/v1/chat/completions` does not preserve native-API behavior: the native adapter sent `temperature` and `num_predict` and ignored `response_format`, and never set `num_ctx`. Anyone who needs those must say so; the way back is a native adapter behind the FX provider boundary, not the legacy executor.
