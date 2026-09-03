# End-to-end test harness

_Added 2026-09-03._

Runs the real `modelrouter` binary as a child process, points it at a mock LLM
provider, and drives it over HTTP the way a client does.

## Why this tier exists

On 2026-09-03 a startup guard was added that refuses to serve when
`auth.jwt_secret` is empty or is the shipped `change-me-jwt-secret` placeholder.
The guard is correct — the Helm chart had been signing every admin session with
an empty secret. But `modelrouter init` writes that placeholder into the config
it generates, and then prints:

```
2. Run: modelrouter migrate
3. Run: modelrouter serve
```

So a fresh install could not start. The quickstart the tool prints led directly
into a refusal.

**All 425 tests passed.** They passed because every one of them constructs
`AppState` by hand and calls `build_router` in-process. Nothing in the suite
executes `main`, `serve`, config resolution, migration execution, or the startup
guards — so a change that makes the binary refuse to boot is structurally
invisible to the entire test suite. The defect reached `main` and was found by
running the binary by hand.

This tier closes that gap. It is the only place where `modelrouter` is tested as
a program rather than as a library.

## How to run it

```bash
# the whole tier
cargo test --test test_e2e_startup --test test_e2e_requests --test test_e2e_accounting -- --ignored

# one file
cargo test --test test_e2e_startup -- --ignored
```

**Do not use a bare `cargo test -- --ignored`.** It also runs
`redis_live_round_trip_scratch_namespace` in `src/router/cache/store.rs`, which
requires a Redis on `127.0.0.1:6379` and fails without one. That test is
unrelated to this tier and predates it; naming the three targets explicitly
avoids it.

Every test is `#[ignore]`d so `cargo test` stays fast for ordinary development.
The default run reports them as ignored rather than skipping them silently:

```
Running tests/test_e2e_startup.rs
test result: ok. 0 passed; 0 failed; 6 ignored; 0 measured; 0 filtered out
```

## Shape

```
test harness ──HTTP──> modelrouter ──HTTP──> mock LLM provider
  (client)          (real child process)      (in-process axum)
                            │                        │
                            ▼                        ▼
                    SQLite in a TempDir     records what it received
```

Two inspection paths make assertions possible that an HTTP response alone
cannot support: the test can ask the mock **what modelrouter sent upstream**,
and can query the database for **what modelrouter recorded**.

| Piece | File | Notes |
|---|---|---|
| Mock provider | `tests/common/mock_llm.rs` | OpenAI shape, programmable responses, records every request |
| Process fixture | `tests/common/e2e.rs` | Starts the real binary, polls `/health`, kills on drop |
| Startup coverage | `tests/test_e2e_startup.rs` | `init`, `migrate`, `serve`, the guards |
| Request coverage | `tests/test_e2e_requests.rs` | Auth, routing to the provider, upstream failures |
| Side-effect coverage | `tests/test_e2e_accounting.rs` | Cost ledger, cache, streaming |

Three properties the fixture enforces, because breaking any of them makes the
suite flaky or destructive:

- **Every instance gets its own `TempDir`.** `~/.modelrouter` is never touched.
  The `init` test overrides `HOME`, because `init` ignores `--config` and always
  writes to the home directory.
- **Every port is discovered by binding `:0`.** No fixed ports, so runs do not
  collide with each other or with a developer's running server.
- **Readiness is polled, never slept.** A fixed sleep is both slower than needed
  and flaky under load. On timeout the failure includes the captured server log,
  so a failing test is diagnosable without re-running it by hand.

The mock is reached through a provider named `mock`, which `ProviderRegistry`
does not recognise and therefore serves with `OpenAICompatAdapter`. That is
deliberate: the harness exercises the real adapter, and no test-only branch
exists anywhere in `src/`.

## What each test proves

| Test | Proves | Req |
|---|---|---|
| `init_then_serve_starts` | The quickstart `init` prints actually reaches a serving process. **This is the regression test for the defect above.** | R5 |
| `placeholder_secret_refuses_to_start` | The guard fires on the shipped placeholder and names the field | R5 |
| `empty_secret_refuses_to_start` | The guard fires on an empty secret — the value the Helm chart shipped | R5 |
| `migrations_apply_to_a_fresh_database` | `migrate` creates the database and its schema | R6 |
| `migrations_are_idempotent` | Running `migrate` twice succeeds, as a restart does | R6 |
| `fixture_starts_a_healthy_server` | The fixture works; everything else depends on it | R1–R3 |
| `valid_key_reaches_provider` | A keyed request reaches the provider, and the provider sees the requested model | R7 |
| `missing_key_is_rejected_before_the_provider` | An unauthenticated request is rejected **and never reaches upstream** | R7 |
| `invalid_key_is_rejected_before_the_provider` | Same for an unknown key | R7 |
| `provider_error_surfaces_and_router_survives` | An upstream 500 becomes an error, and the process is still serving afterwards | R10 |
| `upstream_rate_limit_is_retried` | A 429 is retried rather than passed straight through | R10 |
| `usage_becomes_a_ledger_row` | Provider-reported tokens become a `cost_ledger` row with the right model, provider and counts | R8 |
| `identical_request_reaches_the_provider_once` | With the cache on, an identical eligible request hits upstream once and is marked `HIT` | R9 |
| `without_the_cache_both_requests_reach_the_provider` | Negative control: with the cache off, both requests reach upstream | R9 |
| `streaming_request_returns_an_event_stream` | `stream: true` is forwarded upstream, returns SSE, and the process survives | R10 |

The two rejection tests assert `mock.request_count() == 0`. That is the load-bearing
part: an auth check that ran *after* the upstream call would still return 401
while burning real tokens, and only the mock's record can tell the difference.

## Evidence

Full run, 2026-09-03:

```
     Running tests/test_e2e_accounting.rs
test usage_becomes_a_ledger_row ... ok
test streaming_request_returns_an_event_stream ... ok
test identical_request_reaches_the_provider_once ... ok
test without_the_cache_both_requests_reach_the_provider ... ok
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.76s

     Running tests/test_e2e_requests.rs
test missing_key_is_rejected_before_the_provider ... ok
test invalid_key_is_rejected_before_the_provider ... ok
test valid_key_reaches_provider ... ok
test upstream_rate_limit_is_retried ... ok
test provider_error_surfaces_and_router_survives ... ok
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.67s

     Running tests/test_e2e_startup.rs
test empty_secret_refuses_to_start ... ok
test init_then_serve_starts ... ok
test migrations_apply_to_a_fresh_database ... ok
test fixture_starts_a_healthy_server ... ok
test migrations_are_idempotent ... ok
test placeholder_secret_refuses_to_start ... ok
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.27s
```

Default suite unaffected — `cargo test` reports the e2e files as ignored and its
runtime is unchanged.

### Negative control

A passing test proves nothing unless it can fail. The `init` fix was temporarily
reverted so `init` wrote the placeholder again, and the regression test was run:

```
test init_then_serve_starts ... FAILED

thread 'init_then_serve_starts' panicked at tests/test_e2e_startup.rs:46:5:
init wrote the shipped placeholder secret; `serve` will refuse to start

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 5 filtered out
```

The fix was restored and the test passes again. The tier detects the exact
defect that motivated it.

`without_the_cache_both_requests_reach_the_provider` serves the same purpose for
the cache test: without it, a cache that silently never engaged would be
indistinguishable from one that works.

## Defects this tier found

Building the harness surfaced a second silent-discard defect, of the same class
as the Helm prefix bug:

**`serve` never reads `settings.server.host` or `settings.server.port`.** It
binds from its own `--host`/`--port` flags, which carry clap `default_value`s, so
those flags always have a value and the config is ignored entirely. An operator
setting `port = 9000` in `config.toml` gets 8080, with no warning — even though
`config.example.toml` documents the field and `ServerConfig` defines it. Only
`server.ip_rate_limit_rpm` is read from that struct.

The fixture works around it by passing `--port` explicitly. **The defect is not
fixed** — fixing it would change behaviour for anyone currently relying on the
flag while having a different value in config, so it wants a deliberate decision
rather than a drive-by change.

## Known gaps

- **Postgres is not covered.** `migrations/postgres/` is never executed by any
  code — `sqlx::migrate!("./migrations")` reads only the top-level directory, and
  `PostgresDb::connect` is never called in `src/`. This tier could not test it
  without a running Postgres and a code path that uses it.
- **`/v1/messages` and `/v1/responses` are not covered.** The mock speaks the
  OpenAI shape only. Those routes duplicate the completions pipeline by
  copy-paste, so they are exactly where divergence would hide.
- **Streaming is a smoke check.** That SSE arrives incrementally rather than
  buffered, and that the ledger row is written after the stream ends, are not
  asserted.
- **Not wired into CI.** Run it manually until it has proven stable.
- **One inherent race.** The fixture claims a port by binding and releasing it,
  leaving a window before the child binds. It is far smaller than the collision
  rate of a fixed port, and passing an inherited listener is not something the
  binary supports.
