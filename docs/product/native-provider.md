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

## Known limitations of the native provider

**HTTP proxies: supported through `CHIP_PROXY`, explicitly.** `CHIP_PROXY=http://host:port` sends model calls
through that HTTP `CONNECT` proxy, for both providers (#35). The tunnel carries TLS end to end, so certificate
validation is unchanged; for a TLS-intercepting proxy, also set `SSL_CERT_FILE` to its CA (below).
`CHIP_PROXY_USERNAME` and `CHIP_PROXY_PASSWORD` (both or neither) authenticate to the proxy with Basic; the
password is never printed (errors, `Debug` output and the provider's failure text are scrubbed) and the endpoint
never receives it. `CHIP_NO_PROXY` is a comma-separated list of hosts reached directly (`example.com`,
`.example.com` or `*.example.com`). `chip` reads only its own variables: `HTTP_PROXY`, `HTTPS_PROXY`,
`ALL_PROXY` and `NO_PROXY` are **not** honored, so an inherited shell setting never redirects model traffic.
Refused before anything is sent: a companion variable without `CHIP_PROXY`; a username without a password; a
proxy URL that is not `http://host:port` (no `https://` or SOCKS proxy, no path, no credentials in the URL); and a
keyed plain-`http` endpoint, whose key the proxy would see in the clear. A proxy that cannot be reached, or that
refuses the tunnel (for example `407`), is the model call's `transport` failure with its delivery certainty: there
is no direct connection as a fallback and no second executor. The proxy lives inside the FX provider boundary
(`fx-native-provider`). What is verified: routing through a loopback `CONNECT` proxy, the no-proxy bypass, Basic
credentials reaching only the proxy, a refused or unreachable proxy failing with zero connections to the endpoint
(FX's `tests/proxy.rs`, and `crates/chip-fx-native/tests/proxy.rs` through Chip), and the variable rules
(`provider_selection.rs` tests). **Not verified:** a real corporate proxy, and proxy authentication schemes other
than Basic (not supported).

**Custom root certificates: supported through `SSL_CERT_FILE`, with limits.** If `SSL_CERT_FILE` names a PEM
file, `chip` reads it (the host reads it; the provider never touches the filesystem or the environment) and
trusts **exactly** the certificates in it for `https`, in place of the bundled public roots, the way
`curl --cacert` does. Consequences to know: it applies to every `https` endpoint for that process, so the file
must contain the roots for every endpoint you use; a file that cannot be read is an error naming the variable
(`nothing was run`), never a silent fall back to the public roots; a file with no valid certificate is refused
when the provider is built; certificate validation stays on. Unset or empty keeps the bundled roots. What is
verified: the bundle is accepted or refused as described (unit tests, both providers) and the variable is read
and failed on as described. The TLS handshake itself is tested in FX
(`tests/private_ca.rs` in `fx-native-provider`, against `openssl s_server` with a generated CA): a server signed
by a private CA is refused with the bundled roots and with a different CA, and reached with its CA, directly and
through a proxy.

Other limits, unchanged: plain `http` only for loopback (or a private IP literal with the explicit
`CHIP_ALLOW_INSECURE_LAN_HTTP=1` opt-in, keyless); no client certificates (mTLS); Ollama's own `/api/chat` API is not
supported (below).

## The executor in `chip`

FX is the only executor. `chip work`, `chip serve`, `chip "<prompt>"` and every other model-calling command
(`verify`, `work_demo`, `horizon`, `pax_work`, the micro shadow, `--benchmark-live-reasoner`, the `micro_eval`
bench) build their provider through `provider_selection.rs`, which is the one place that constructs it:

```
chip work "<goal>" --provider anthropic --model <model>
```

`--executor fx` and `CHIP_EXECUTOR=fx` are still accepted (they select the only executor), so a deployment
that set them keeps working. **`--executor legacy`, `CHIP_EXECUTOR=legacy` and any other value fail before any
provider request** with `FX is the only supported executor`; both the flag and the environment are checked,
so a valid flag does not hide a stale environment value. Nothing falls back to anything.

The same `CHIP_PROVIDER`, `CHIP_MODEL` and `CHIP_ENDPOINT` select the provider; the endpoint must be the full
URL (`.../v1/chat/completions` for OpenAI-compatible servers, `.../v1/messages` for Anthropic), from which the
API root is derived. Keys come from `CHIP_API_KEY` if set, otherwise `ANTHROPIC_API_KEY` or `OPENAI_API_KEY`
(see Credentials above). Anything the native path cannot carry is a configuration error that names it, before
any request: `CHIP_ENABLE_THINKING` on the Anthropic provider (which has no such field), and an endpoint that
does not end in the expected path. `crates/chip-fx/tests/retirement.rs` fails if `chip-legacy-http` returns, is
imported, or is named by a script or workflow.

## Behavior of the FX executor

`crates/chip-cli/tests/fx_executor.rs` runs the executor against one loopback server that answers in the
format FX asks for (an event stream), and pins:

* the response text and token usage, for OpenAI-compatible and Anthropic;
* the wire fields `model`, `messages` (consecutive system messages merged into one), `max_tokens` and
  `temperature` (a configured `0.0` is sent, not dropped), Anthropic's top-level `system`, authentication
  (`Bearer`, `x-api-key`, `anthropic-version`) and the Anthropic workspace header and fallback cap (1024);
* JSON-object output as `response_format: json_schema` with `{"type":"object"}` (accepted by OpenAI and LM
  Studio; not verified on OpenRouter models) and that Anthropic ignores the flag;
* failures for 401, 403, 429, 500 and 503: their class, exactly one request (no retry), and the key never in
  the error; structured detail (kind, status, retry-after, partial output, delivery certainty
  `definitely_unsent` / `possibly_sent` / `confirmed_response`);
* a stream cut mid-way is a failure with the partial text, never a completion;
* what it refuses by name before any request (an endpoint without the provider path, `CHIP_ENABLE_THINKING` on
  Anthropic, a missing key, plain `http` off loopback);
* `CHIP_ENABLE_THINKING` as the top-level `chat_template_kwargs.enable_thinking` field on OpenAI-compatible
  endpoints.

## Release evidence still outstanding

* **Live checks.** Anthropic and LM Studio were run by the owner and reported clean; the raw output and audit have
  not been added to the repository. **OpenRouter has not been run** (a release gate, tracked separately).
  Everything else above is mocked transport; TLS has no test.
* The horizon live run needs real Compute before it asks a model; its model request was not exercised end to end,
  only its provider selection.

## Ollama

Decision (2026-10-16, repository owner): **Ollama's own `/api/chat` API is not a supported provider** and no native
implementation is built. Ollama's OpenAI-compatible endpoint is the supported path:

```
chip work --provider openai-compatible --endpoint http://127.0.0.1:11434/v1/chat/completions --model <model> "<goal>"
```

`CHIP_PROVIDER=ollama` (and `--provider ollama`, `CHIP_MICRO_PROVIDER=ollama`) fails before any request with a
configuration error that names this replacement; nothing falls back. `CHIP_OLLAMA_ENDPOINT` is no longer read.
Supporting `/v1/chat/completions` does not preserve native-API behavior: the former native adapter sent
`temperature` and `num_predict`, ignored `response_format`, and never set `num_ctx`. Anyone who needs those must say
so; the way back is a native adapter behind the FX provider boundary.

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
