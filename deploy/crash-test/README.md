# Local crash test

Proves what callers see when a replica dies or drains, without containers or
root: N `modelrouter serve` processes on one PostgreSQL and one Redis, behind
`lb.py` (a small balancer that removes a backend after 3 failed 1 s readiness
probes, like a readinessProbe, and with `retry` re-sends requests that failed
before any response byte, like an ingress `proxy_next_upstream error`), under
steady load from `load.py` against `stub_upstream.py`. At one third of the run
one replica is killed (`kill`: SIGKILL, replaced after 5 s) or drained
(`drain`: SIGTERM, replaced once it exits). The summary counts failures by kind
and checks that every successful request left a cost row.

```bash
cargo build --release --features postgres
MODELROUTER_BIN=target/release/modelrouter \
CRASH_PG_URL=postgres://user@127.0.0.1:5432/scratch \
CRASH_REDIS_URL=redis://127.0.0.1:6379 \
CRASH_DRAIN_DELAY=5 \
deploy/crash-test/run.sh kill retry 30
```

Arguments: scenario (`kill` | `drain`), balancer mode (`retry` | `noretry`),
seconds. Environment: `CRASH_REPLICAS` (3), `CRASH_WORKERS` (16),
`CRASH_DRAIN_DELAY` (`[server] drain_delay_secs`, 0), `CRASH_BASE_PORT`
(18080), `CRASH_LOG` (`RUST_LOG` for the replicas, `error`). Python 3 standard
library and `curl` only.

## Results (2026-11-22, 3 replicas, 16 workers, 30 s, 30% streamed)

| build | scenario | balancer | failed | ok without a cost row |
|---|---|---|---|---|
| before #176 | SIGTERM | no retry | 17 / 665 (503) | 3 |
| before #176 | SIGTERM | retry | 22 / 564 (503) | 1 |
| #176, drain delay 5 | SIGTERM | no retry | 0 / 637 | 0 |
| #176, drain delay 5 | SIGTERM | retry | 0 / 663 | 0 |
| before #176 | SIGKILL | no retry | 35 / 581 (502 until the probe drops it, 3 cut streams) | 4 |
| before #176 | SIGKILL | retry | 3 / 449 (cut streams) | 0 |
| #176 | SIGKILL | retry | 1-5 / ~600 (cut streams) | 0, 0, 34 over three runs |

Reading it: a graceful stop (rollout, scale-down) is now invisible to callers.
A crash costs exactly the streams that were mid-flight on the dead replica,
provided the ingress retries connection failures (#172); without that, every
request routed to the dead pod until its endpoint is removed fails. A crash
can also lose the cost rows of requests that had already been answered
(#173).
