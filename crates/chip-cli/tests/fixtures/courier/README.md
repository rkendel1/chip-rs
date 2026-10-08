# courier

A small request-dispatch client. It resolves each request to a route, applies rate limiting and a
circuit breaker, runs it through a transport with retries, and records the outcome in a journal.

```text
Client::send
  -> prepare headers / auth
  -> RouteTable::resolve        (per-route settings, decided at dispatch)
  -> InflightGate, RateLimiter, CircuitBreaker   (admission)
  -> Executor::execute          (attempts, backoff, Retry-After, deadlines)
  -> Journal + metrics + circuit snapshot
```

The library never opens a socket: requests go through the `Transport` trait. The `courier` binary
uses an in-process loopback transport so it runs anywhere.

```sh
cargo test
cargo run --bin courier -- send GET http://example.test/status/200
cargo run --bin courier -- --config courier.conf config show
```

* [docs/configuration.md](docs/configuration.md): every section and key
* [docs/architecture.md](docs/architecture.md): module layout and the decisions behind it
* [docs/operations.md](docs/operations.md): what operators rely on

Every test is deterministic: time goes through a `Clock`, and `ManualClock` never really sleeps.
