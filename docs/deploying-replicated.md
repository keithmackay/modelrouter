# Running several replicas on PostgreSQL

_Added 2026-11-05._

This guide is for operating modelrouter as two or more replicas in Kubernetes with robust failover and zero-downtime upgrades. One replica on SQLite stays the default and needs none of this. Each section names the setting that matters and why.

## What you get

| Event | Behaviour |
|---|---|
| A pod dies | The Service drops its endpoint; the other replicas serve; the Deployment replaces it. |
| A node or zone is lost | Topology spread put a replica elsewhere; there is no disk to reattach. |
| A new image cannot start | `serve --check-config` fails, or readiness never passes; old pods keep serving. |
| A migration fails | The new pod never becomes Ready; the old binary keeps running on the expand-only schema. |
| PostgreSQL is unavailable | Readiness fails and the pod leaves the Service until it returns. |
| Redis is unavailable | Cache misses; limits fall back to each replica's share; admin login fails until it returns. |

## 1. Build with the PostgreSQL feature

```bash
cargo build --release --features postgres
# or the published image
ghcr.io/keithmackay/modelrouter:latest-postgres
```

A binary built without the feature refuses a config that sets `postgres_url`, so a mismatch fails loudly at start rather than silently opening SQLite beside the real store.

## 2. Point the store at PostgreSQL

```toml
[database]
postgres_url = "postgres://modelrouter:secret@db.internal:5432/modelrouter"
```

`serve` and every CLI command open this database and bring its schema to head on start. sqlx takes an advisory lock for the migration run, so replicas starting together apply each step once; the others wait and then find nothing to do. Pool sizing is per replica: sqlx's default pool is ten connections, so budget `replicas x 10` connections on the server, plus one for each CLI invocation and the migrate init container.

Moving an existing SQLite store: stop writes, `db export`, `db import` into the PostgreSQL config, repoint, verify a key. The README section on `db export` has the exact steps.

## 3. Migrations stay expand-only

Every migration adds and never takes away in the release that stops using a shape, so an older binary runs on a newer schema during the rollout and after a rollback. `migrations/README.md` has the rule and the `-- contract-step:` marker for a later removal; `cargo test` and CI enforce it. For an upgrade this means: roll forward, and if you must roll back, roll the image back. No data restore.

## 4. Share the cache and the limits through Redis

```toml
[cache]
enabled = true
backend = "redis"
redis_url = "redis://redis.internal:6379"
namespace = "modelrouter"

[cluster]
replicas = 3
backend = "redis"      # limits, session pins and pending admin logins
# redis_url and namespace default to the [cache] values
```

`[cluster] replicas` alone divides the in-memory limits by the replica count. `backend = "redis"` makes them exact and shared, keeps session affinity pins visible to every replica, and lets an admin login started on one replica finish on another. The Helm chart passes `replicaCount` through for you.

## 5. Wire the three probes

| Probe | Path | Suggested timing | Why |
|---|---|---|---|
| startup | `/health/ready` | period 5 s, failure threshold 60 | Migrations run before the listener binds; a first start answers nothing until they finish. |
| readiness | `/health/ready` | period 5 s, failure threshold 3 | Store reachable, schema at head, every provider adapter built, not draining. 503 removes the pod from the Service without restarting it. |
| liveness | `/health/live` | period 10 s, failure threshold 3 | No I/O. Only a hung process fails it. |

Never point liveness at `/health/ready`: a store outage would then restart every replica in a loop instead of letting them wait it out.

## 6. Let the drain finish long streams

On SIGTERM the router flips readiness to 503, keeps the listener open, answers new requests with 503 and `Connection: close`, lets in-flight requests finish to their last byte, and exits as soon as nothing is in flight or `[server] shutdown_grace_secs` elapses. The grace defaults to the longest configured tier timeout. The pod's `terminationGracePeriodSeconds` must be at least as large or the kubelet kills the process first; the chart sets both. An idle replica exits in milliseconds, so a long ceiling costs nothing in the common case.

## 7. Rollout settings

```yaml
replicaCount: 3
rollout:
  maxSurge: 1
  maxUnavailable: 0
podDisruptionBudget:
  enabled: true
  minAvailable: 1
topologySpread:
  enabled: true
```

`maxSurge: 1` with `maxUnavailable: 0` starts one new pod, waits for its startup and readiness probes, and only then stops one old pod. A new build that cannot start therefore never reduces capacity. The PodDisruptionBudget keeps one pod serving through node drains; enable it only with two or more replicas or a drain can never proceed. Topology spread places replicas across zones and nodes with `ScheduleAnyway`, so a small cluster still schedules.

## 8. Prove a build before it takes traffic

```bash
modelrouter serve --check-config
```

Loads the config, checks the signing secret and OIDC role, opens the database and counts pending migrations without applying them, and builds every provider adapter without calling a provider. Exit 0 or a named failure. Run it as a pre-deploy job against the new image, or as a CI step against a candidate config.

## Checklist for the first replicated rollout

1. PostgreSQL provisioned; `postgres_url` injected as a secret; image built with `--features postgres`.
2. Existing SQLite store moved with `db export` / `db import` in one quiesced window.
3. Redis reachable; `[cache]` and `[cluster]` pointed at it.
4. Chart values: `replicaCount >= 2`, probes as above, `rollout`, `podDisruptionBudget`, `topologySpread`, `terminationGracePeriodSeconds`.
5. `serve --check-config` green from the new image.
6. Roll out; watch `/health/ready` on the new pod before the old one stops.
