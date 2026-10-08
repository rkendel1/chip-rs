# Configuration reference

Configuration is layered: built-in defaults, then the file, then the environment. Later layers win
key by key. `COURIER_<SECTION>_<KEY>` sets `section.key` (for example `COURIER_CLIENT_TIMEOUT=2s`);
route sections are file-only. Unknown sections and keys produce warnings and are otherwise ignored.

Durations need a unit: `ms`, `s`, `m` or `h`.

## [client]

| key | default | meaning |
| --- | --- | --- |
| `base_url` | none | informational; requests carry full URLs |
| `timeout` | `10s` | bound on each attempt |
| `deadline` | none | bound on the whole request, retries and waits included; must not be shorter than `timeout` |
| `max_wait` | none | longest single wait; a longer wait (backoff or a server `Retry-After`) fails the request with `wait_too_long` instead of sleeping |
| `max_inflight` | `32` | requests allowed in flight at once (1 to 1024) |
| `user_agent` | `courier/0.7` | sent as `User-Agent` unless the caller set one |

Retries: every request gets three attempts, waiting 100ms, then 200ms between them (doubling, never
more than 5s). A server `Retry-After` longer than that wait is honored.

## [auth]

| key | default | meaning |
| --- | --- | --- |
| `token` | none | credential sent in `Authorization` |
| `scheme` | `bearer` | `bearer` or `basic` |

## [journal]

| key | default | meaning |
| --- | --- | --- |
| `enabled` | `false` | append one line per finished request |
| `path` | none | journal file; required when enabled. Circuit state is saved beside it as `<path>.state` |

## [circuit]

| key | default | meaning |
| --- | --- | --- |
| `enabled` | `true` | open a route after repeated failures |
| `threshold` | `5` | consecutive failed requests that open a route |
| `cooldown` | `30s` | how long a route stays open before one probe is allowed |

The breaker counts requests, not attempts.

## [rate_limit]

| key | default | meaning |
| --- | --- | --- |
| `enabled` | `false` | admit requests through a token bucket |
| `rate` | `100` | tokens added per second |
| `burst` | `20` | bucket size |

## [route.NAME]

A route applies to URLs whose path starts with its `prefix`, on segment boundaries; the longest
matching prefix wins. A route sets only what it names: everything else is inherited from `[client]`.

| key | meaning |
| --- | --- |
| `prefix` | required; path prefix starting with `/` |
| `timeout` | replaces `[client] timeout` |
| `deadline` | replaces `[client] deadline` |
| `max_wait` | replaces `[client] max_wait` |
| `max_inflight` | replaces `[client] max_inflight` |

Route settings are resolved when a request is dispatched, so `Client::set_route_overrides` affects
the next request.
