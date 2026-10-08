# Architecture and decisions

## Layout

| module | responsibility |
| --- | --- |
| `config` | parse, layer, type and validate configuration |
| `routes` | prefix matching; per-route overrides of execution settings |
| `exec` | one request becomes attempts: classification, retry budget, backoff, deadline |
| `middleware` | request preparation, admission control (in-flight, rate, circuit) |
| `state` | journal and key/value store on disk |
| `transport` | the network boundary and its test doubles |
| `client` | assembles the above behind `Client::send` |
| `cli` | the `courier` binary |

## Decisions

**ADR-001: no sockets in the library.** All I/O goes through `Transport`; time goes through `Clock`.
Tests never sleep.

**ADR-002: the executor is immutable once built.** Variation per request is expressed through the
route table, which is consulted at dispatch time.

**ADR-003: the circuit breaker counts requests.** A request that succeeds on its last attempt is a
success; one that exhausts its attempts is one failure.

**ADR-004: configuration is lenient.** Unknown keys and sections are warnings. A configuration
written for a newer release, or containing a typo, must still load: operators roll configuration
out before code and need an old binary to survive it.

**ADR-005: server hints are advisory but honored.** `Retry-After` is followed unless a deadline or
`max_wait` forbids the wait. The hint never shortens the client's own backoff.

**ADR-006: routes inherit.** A route names only what it changes.

**ADR-009: safety-relevant settings fail closed.** Where a wrong value could make the client behave
unsafely (credentials, admission control), an invalid value is an error rather than a fallback to a
default. Values that are merely tuning knobs follow ADR-004.
