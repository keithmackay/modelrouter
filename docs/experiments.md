# Running a model experiment through modelrouter

This guide is for a client application that wants to answer a question like
"is a cheaper model good enough for our checkout assistant, and what does it
save?" by sending real traffic down two or more arms and reading the
difference back from the router.

There are two ways to do it:

- **A router-managed experiment** (Part 1). Create the experiment once, with
  named variants; put one header on each request; the router pins every
  bound request to its variant's model, stamps every row it writes with the
  experiment and variant, and assembles a per-variant and per-run results
  document — cost, tokens, turns, wall-clock span, latency, failures and the
  outcomes the application reports back. This is the path for an application
  that wants the router to own assignment and bookkeeping.
- **A comparison of arms you label yourself** (Part 2). The application picks
  the model per request and tags it; the router compares any two tag values,
  correlation ids, models or providers over a time window. Nothing is
  created up front and nothing changes routing.

Both end at the same measurement: the router records what each request
cost, how long it took and whether it failed. It never judges answers. The
application owns quality; the router owns the ledger.

---

# Part 1 — Router-managed experiments

## 1. What an experiment is

An experiment has a unique **name**, two to sixteen **variants**, an
**expiry**, a **status** (`active` or `closed`) and a content-retention
setting. Each variant is a label (`control`, `candidate.v2`, …) and an
**overlay**: a map from the model name a caller requests to the target it is
sent to instead.

```json
{
  "control":   { "fast": "fast" },
  "candidate": { "fast": "anthropic/claude-haiku-4-5" }
}
```

A request bound to `candidate` that asks for `fast` goes to
`anthropic/claude-haiku-4-5`; one bound to `control` goes wherever `fast`
normally resolves. A requested name the overlay does not mention routes as
usual (and is still stamped with the variant). An empty overlay (`{}`) is a
legal variant — it means "route exactly as today", which is what a control
arm usually wants.

Every target is **resolved and pinned at creation**: the alias or
`provider/model` expression is turned into a concrete `provider/model` pair
and stored. Changing an alias later does not move a running experiment.
Creation is refused when any target cannot be accounted for:

| Refused when the target… | Message (400) |
|---|---|
| is a load balancer pool | `… is a load balancer pool; an experiment must pin one provider/model` |
| is neither an alias nor `provider/model` (it would fall through to `routing.default_model`) | `… is not an alias or provider/model and would be substituted with the default model` |
| resolves to a provider that is not configured | `… resolves to unconfigured provider 'x'` |
| resolves to a model with no `[pricing]` entry | `… resolves to 'provider/model', which has no pricing entry` |

Each message starts with `variants: variant '<label>' key '<name>'
target '<expr>'`, so the caller knows exactly which entry to fix. The pricing
check is deliberate: an experiment whose cost column is a silent zero would
always "win".

## 2. Create it

Creation needs a **superadmin** JWT (see [Credentials](#credentials)); reads
need any admin role, viewers included.

### `POST /admin/api/experiments`

```bash
curl -s -X POST http://router:8080/admin/api/experiments \
  -H "Authorization: Bearer $JWT" -H 'Content-Type: application/json' -d '{
    "name": "checkout-haiku",
    "variants": {
      "control":   { "fast": "fast" },
      "candidate": { "fast": "anthropic/claude-haiku-4-5" }
    },
    "expires_at": "2026-10-01T00:00:00Z",
    "content_retention_days": 30,
    "retain_content": false,
    "feed_learning": false,
    "allowed_user_ids": [4, 9]
  }'
```

| Field | Required | Rules |
|---|---|---|
| `name` | yes | 1–128 characters, unique across all experiments (closed ones included) |
| `variants` | yes | object of label → overlay; 2–16 labels; label matches `[A-Za-z0-9_.-]{1,64}`; each overlay has at most 32 entries; keys and targets 1–128 characters |
| `expires_at` | yes | RFC3339 timestamp in the future, or the number `0` for never. **No default** — a bare experiment that runs forever has to be asked for |
| `content_retention_days` | yes | integer 0–3650; days after close that retained content is kept, `0` = forever. Required even when `retain_content` is false |
| `retain_content` | yes | boolean; see [Content retention](#8-content-retention). `true` requires a finite `expires_at` |
| `feed_learning` | no (default `false`) | stored and returned; nothing reads it yet |
| `allowed_user_ids` | no (default `[]` = every key) | at most 64 user ids; each must exist. Only these users' keys may bind |

`201 Created` returns the stored row:

```jsonc
{
  "id": 3,
  "name": "checkout-haiku",
  "variants": {
    "candidate": { "fast": { "target": "anthropic/claude-haiku-4-5", "provider": "anthropic", "model": "claude-haiku-4-5" } },
    "control":   { "fast": { "target": "fast", "provider": "openai", "model": "gpt-4o-mini" } }
  },
  "allowed_user_ids": [4, 9],
  "status": "active",
  "feed_learning": false,
  "expires_at": 1790812800,          // unix seconds; 0 = never
  "created_at": "2026-09-04T21:10:44Z",
  "closed_at": null,
  "retain_content": false,
  "content_retention_days": 30
}
```

Note that `expires_at` is *sent* as RFC3339 and *returned* as unix seconds.
Every field is echoed back so the application can record exactly what was
pinned. A `400` names the field at fault (`expires_at is required`,
`variants must have 2-16 entries, got 1`, `name 'checkout-haiku' is already
taken`, `allowed_user_ids: no user with id 12`, …). Nothing on a row can be
edited afterwards; the only later transition is closing.

The other endpoints:

| Endpoint | Role | Returns |
|---|---|---|
| `GET /admin/api/experiments?status=active\|closed\|all` | admin | `{"experiments": [row, …]}`; default `active` |
| `GET /admin/api/experiments/:id` | admin | the row; `400 no experiment with id N` |
| `POST /admin/api/experiments/:id/close` | superadmin | the row after closing; `400 experiment N is already closed` |
| `GET /admin/api/experiments/:id/results` | admin | the [results document](#5-read-the-results) |

Creating and closing write an `experiment.create` / `experiment.close` entry
to the audit log (`/admin/audit`) with the full row, `never` spelled out
where the stored value is `0`.

### From the dashboard

`/admin/experiments` (nav: **Experiments**) lists every experiment with its
status, variants, expiry and retention badge, and a **Close** button that
asks for confirmation naming the experiment and, for a retaining one, what
happens to its content. Superadmin sessions also get the create form; expiry
and retention are required fields there too (the expiry picker has no
preselected value). Each row's **Results** button loads its results
panels in place, 50 runs per page.

### From the CLI

The CLI writes to the database directly, so it works without a running
server and without a JWT. **A running server picks up a CLI-created or
CLI-closed experiment within 60 seconds** (the lifecycle tick); the REST and
dashboard paths take effect immediately.

```bash
modelrouter experiment add --name checkout-haiku \
  --variant 'control=fast:fast' \
  --variant 'candidate=fast:anthropic/claude-haiku-4-5' \
  --expires-at 2026-10-01T00:00:00Z \
  --content-retention-days 30 \
  --allow-user checkout-svc            # optional; by user name, repeatable
  # --retain-content  --feed-learning  # optional flags

modelrouter experiment list [--status active|closed|all] [--format table|csv|json]
modelrouter experiment close --id 3
modelrouter experiment results --id 3 [--limit 200] [--offset 0] [--format table|csv|json]
```

`--variant` takes `LABEL=KEY:TARGET[,KEY:TARGET...]`; an empty overlay is
`--variant control=`. `--expires-at` and `--content-retention-days` are
required flags with no default; `--expires-at never` is the CLI spelling of
`0`. The CLI builds the same request body the endpoint takes and applies the
same gate, so it refuses the same inputs with the same messages.
`experiment results --format json` prints the endpoint document verbatim.

## 3. Send the traffic

Add one header to each chat completion:

```text
x-modelrouter-experiment: 3               # router assigns the variant
x-modelrouter-experiment: 3:candidate     # caller names the variant
```

The value is `<id>` or `<id>:<label>` — a positive integer, optionally a
colon and a label in `[A-Za-z0-9_.-]{1,64}`; at most 128 bytes; at most one
copy of the header.

**Which variant.** With `<id>:<label>` the request binds to that label
(`400` if the experiment has no such variant). With `<id>` alone the router
assigns one from the body's `session_id`: the variant is
`labels_sorted_bytewise[FNV-1a-64("<id>:<session_id>") mod n]`, so the same
session lands on the same variant for the life of the experiment on every
router instance, with no shared counter. An id-only header on a body without
a string `session_id` is a `400`. Assign per **session**, not per request —
a conversation that changes model mid-way measures nothing.

**A correlation id is required.** Every bound request must carry
`attribution.correlation_id` (body) or `X-Attribution-Correlation-Id`
(header) — the id of the **run** the request belongs to (a checkout, a
ticket, a batch item). It is how requests are grouped into runs, how turns
and wall-clock span are counted, and the key the application later reports
an outcome against. Use one id per run and reuse it for every turn of that
run.

```json
{
  "model": "fast",
  "stream": false,
  "session_id": "sess-8f1c",
  "messages": [{ "role": "user", "content": "…" }],
  "attribution": { "correlation_id": "checkout:sess-8f1c:2026-09-04" }
}
```

With the OpenAI SDKs, `session_id` and `attribution` go through `extra_body`
(Python) / `extraBody` (TypeScript) and the experiment header through
`extra_headers` / `defaultHeaders`.

**What changes for a bound request.** The variant's overlay decides the
model, and the adaptive layers stand aside so the measurement is of the
pinned model and nothing else:

- no complexity downgrade;
- no load-balancer pool — a pinned target that is a pool name is refused
  at creation, and a requested name that is a pool is a `400` at request
  time (`'<name>' is a load balancer pool; experiments must pin a concrete
  provider/model`);
- no session affinity;
- no response cache — bound requests neither read nor write it, so a cache
  hit never appears in an experiment and `saved_usd` is always zero;
- no fallback — if the pinned model fails, that failure *is* the result and
  is recorded against the variant; a substitute answering would be
  attributed to a model that did not answer.

The prompt-log row records the **requested** name (`fast`) as its request
model and the pinned model as its routed model; the cost ledger records the
pinned model, which is what the per-model breakdown in the results shows.
The `x-no-log` header is honoured as always.

**Where.** Experiments run on `POST /v1/chat/completions` only. Every other
`/v1/*` endpoint — `/v1/messages`, `/v1/responses`, `/v1/embeddings`,
`/v1/feedback`, … — refuses the header with `400 x-modelrouter-experiment is
not supported on this endpoint`, so a misrouted call is a visible error, not
unmarked traffic.

**Send `"stream": false`.** Streamed calls estimate token counts locally;
those rows are counted in `estimated_rows` and their cost is an estimate.

### The 400 catalogue

Binding runs before any provider is contacted and does no I/O; the checks
run in this order and the first failure is returned. Header text is echoed
only after it has passed the character-set check.

| Message | Cause |
|---|---|
| `x-modelrouter-experiment must appear at most once` | duplicate header |
| `x-modelrouter-experiment must be at most 128 bytes` | too long |
| ``x-modelrouter-experiment must be `<id>` or `<id>:<label>` (label: [A-Za-z0-9_.-], at most 64 characters)`` | bad grammar or characters |
| `x-modelrouter-experiment: '<x>' is not a positive experiment id` | id part not a positive integer |
| `x-modelrouter-experiment: label must be at most 64 characters` | label too long |
| `x-modelrouter-experiment: experiment N not found` | no such id |
| `x-modelrouter-experiment: experiment N is closed` | closed by an operator or by expiry |
| `x-modelrouter-experiment: experiment N has expired` | `expires_at` has passed but the lifecycle tick has not closed it yet (checked per request, so the boundary is exact) |
| `x-modelrouter-experiment: user U is not allowed in experiment N` | `allowed_user_ids` is set and does not include the caller |
| `attribution.correlation_id is required when x-modelrouter-experiment is set` | no correlation id |
| `x-modelrouter-experiment: experiment N has no variant '<label>'` | unknown label |
| `session_id is required when x-modelrouter-experiment names no variant` | id-only header, no `session_id` |
| `session_id must be a string` | `session_id` present but not a string |
| `x-modelrouter-experiment is not supported on this endpoint` | header on a non-chat endpoint |

An unknown experiment or variant is a `400`, never silent normal routing: a
typo in the header must not quietly put a run in the wrong bucket.

## 4. Report how each run went

The router sees every request in a run but never whether the run succeeded —
only the application knows that. Without it, results can only say which arm
was cheaper. `POST /v1/feedback` closes the loop; it takes the caller's
ordinary API key, not an admin JWT.

```bash
curl -s -X POST http://router:8080/v1/feedback \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' -d '{
    "correlation_id": "checkout:sess-8f1c:2026-09-04",
    "outcome": "success",
    "score": 0.8,
    "rating": 4,
    "note": "order placed"
  }'
```

| Field | Required | Rules |
|---|---|---|
| `correlation_id` | yes | the run's attribution correlation id exactly as sent on its requests (1–128 printable ASCII) |
| `outcome` | yes | `success` or `failure` — nothing else, so a misspelling cannot form its own bucket |
| `score` | no | number in `[0, 1]` |
| `rating` | no | integer 1–5 |
| `note` | no | at most 1024 characters of bounded metadata (a reason code, a ticket id) — never prompt or response content |

Returns `200` with the stored outcome row. One outcome per `(user,
correlation_id)`: a later report for the same run **replaces** the earlier
one, so an application can post a provisional `failure` and correct it. The
outcome is stamped with the experiment and variant of the run's earliest
bound request, which is how results group it; a run that was never bound is
still accepted and simply does not appear in any experiment.

The run must already be visible under the caller's key — the same
correlation id under another key is a different run and cannot be reported
against or revealed. **Cost rows are written asynchronously**, so a report
sent in the same instant as the run's last response can race it and get
`400 correlation_id '…' has no recorded requests under this API key yet;
requests are logged asynchronously, so retry shortly if the run just
finished`. Treat that one message as retryable (a second or two is plenty);
every other `400` names the field to fix. A run whose every request failed
has no ledger row but does have failure rows, and can be reported.

## 5. Read the results

### `GET /admin/api/experiments/:id/results`

```text
GET /admin/api/experiments/3/results?limit=200&offset=0
Authorization: Bearer <jwt>
```

| Parameter | Default | Rules |
|---|---|---|
| `limit` | 200 | runs per page, 1–1000 (`400 limit must be an integer between 1 and 1000`) |
| `offset` | 0 | runs to skip (`400 offset must be an integer of at least 0`) |

The aggregates cover the whole experiment regardless of paging; only
`runs.items` is a page. There is no time window: the experiment's own life
is the window. The document, for the example above after a few days,
abridged. Field names and order are what the endpoint returns; the CLI's
`--format json` is the same document.

```jsonc
{
  "experiment": { "id": 3, "name": "checkout-haiku", "status": "active", … },   // the row, as returned by GET /admin/api/experiments/3
  "computed_at": "2026-09-08T09:15:02Z",     // figures are read live, never frozen — a price fix or a purge moves them
  "variants": [
    {
      "label": "candidate",
      "runs": 41,                              // runs whose earliest bound request was this variant
      "mixed_runs": 0,                         // of those, runs also seen under another variant
      "requests": 121,                         // ledger rows stamped with this variant
      "unbound_requests": 0,                   // rows sharing a run's correlation id but sent without the header — counted, never merged
      "turns": 121,                            // requests of the runs attributed here (== requests unless runs are mixed)
      "cost_usd": 0.1573,
      "saved_usd": 0.0,                        // always 0: bound requests never hit the cache
      "tokens": { "prompt": 97900, "completion": 34300, "total": 132200 },
      "estimated_rows": 0,                     // rows whose tokens were estimated (streamed calls)
      "failures": 0,                           // failure-log rows stamped with this variant
      "latency": { "samples": 121, "mean_ms": 980.6, "p50_ms": 951, "p95_ms": 1610 },  // null when latency_samples == 0
      "latency_samples": 121,                  // prompt-log rows with a measured latency; can be fewer than requests
      "per_run":     { "turns": 2.95, "cost_usd": 0.003837, "tokens": 3224.4, "span_secs": 36.8 },   // null when runs == 0
      "per_request": { "cost_usd": 0.0013, "tokens_in": 809.1, "tokens_out": 283.5 },              // null when requests == 0
      "models": [                              // the pinned (backing) models this variant actually called
        { "model": "claude-haiku-4-5", "requests": 121, "cost_usd": 0.1573, "saved_usd": 0.0,
          "tokens": { "prompt": 97900, "completion": 34300, "total": 132200 }, "estimated_rows": 0, "unpriced": false }
      ],
      "unpriced": false,                       // true if any model here has no pricing entry
      "unpriced_models": [],
      "outcomes": {                            // from POST /v1/feedback; null rates when nothing was reported
        "reported": 39, "success": 33, "failure": 6, "success_rate": 0.8462,
        "mean_score": 0.77, "score_samples": 39, "mean_rating": null, "rating_samples": 0
      }
    },
    {
      "label": "control",
      "runs": 40, "mixed_runs": 0, "requests": 118, "unbound_requests": 2, "turns": 118,
      "cost_usd": 0.2214, "saved_usd": 0.0,
      "tokens": { "prompt": 96500, "completion": 30100, "total": 126600 },
      "estimated_rows": 0, "failures": 1,
      "latency": { "samples": 118, "mean_ms": 1420.3, "p50_ms": 1388, "p95_ms": 2210 }, "latency_samples": 118,
      "per_run": { "turns": 2.95, "cost_usd": 0.005535, "tokens": 3165.0, "span_secs": 41.2 },
      "per_request": { "cost_usd": 0.001876, "tokens_in": 817.8, "tokens_out": 255.1 },
      "models": [ { "model": "gpt-4o-mini", "requests": 118, … } ],
      "unpriced": false, "unpriced_models": [],
      "outcomes": { "reported": 38, "success": 31, "failure": 7, "success_rate": 0.8158,
                    "mean_score": 0.74, "score_samples": 38, "mean_rating": null, "rating_samples": 0 }
    }
  ],
  "totals": {                                  // sums of the per-variant columns
    "runs": 81, "mixed_runs": 0, "requests": 239, "unbound_requests": 2, "turns": 239,
    "cost_usd": 0.3787, "saved_usd": 0.0,
    "tokens": { "prompt": 194400, "completion": 64400, "total": 258800 },
    "estimated_rows": 0, "failures": 1, "latency_samples": 239,
    "outcomes": { "reported": 77, "success": 64, "failure": 13, "success_rate": 0.8312,
                  "mean_score": 0.7552, "score_samples": 77, "mean_rating": null, "rating_samples": 0 }
  },
  "retained_content_bytes": 4182233,           // only present when the experiment retains content
  "runs": {
    "total": 81, "limit": 200, "offset": 0,
    "items": [                                 // newest ledger activity first; runs with no ledger rows (every request failed) follow
      {
        "user_id": 4,
        "correlation_id": "checkout:sess-8f1c:2026-09-04",
        "variant": "candidate",                // variant of the run's earliest bound request
        "mixed": false,
        "requests": 3, "unbound_requests": 0, "turns": 3,
        "cost_usd": 0.0041, "saved_usd": 0.0,
        "tokens": { "prompt": 2410, "completion": 860, "total": 3270 },
        "estimated_rows": 0, "failures": 0,
        "latency": { "samples": 3, "mean_ms": 1012.7 }, "latency_samples": 3,
        "span_secs": 38.4,                     // earliest to latest bound request
        "first_at": "2026-09-04T14:02:11Z", "last_at": "2026-09-04T14:02:49Z",
        "outcome": { "outcome": "success", "score": 0.8, "rating": 4, "note": "order placed",
                     "reported_at": "2026-09-04T14:03:02Z" }   // null when the run has no report
      },
      …
    ]
  }
}
```

**How figures are attributed.** Request-level figures — `requests`, cost,
tokens, `estimated_rows`, `failures`, latency, the model breakdown — belong to
the variant stamped on each row, so they always sum to the totals. Run-level
figures — `runs`, `turns`, `unbound_requests`, span, outcomes — belong to
the variant of the run's *earliest* bound request. The two disagree only for
a **mixed run**, one whose requests were bound to more than one variant
(the caller switched labels mid-run, or a session id changed); those are
counted in `mixed_runs` and flagged `mixed` on the row rather than silently
merged.

### From the CLI and the dashboard

```bash
modelrouter experiment results --id 3                   # table: header, variants, a totals row, the run page
modelrouter experiment results --id 3 --format csv      # the variant table and the run table, blank-line separated
modelrouter experiment results --id 3 --format json     # the document above
```

On `/admin/experiments`, selecting an experiment renders the same document
as panels: a header with status, expiry and retention; one card per variant
with an **unpriced** badge where it applies; the per-model table; and the
paged run table with each run's outcome.

## 6. Compare two variants

The comparison page and endpoint from Part 2 accept a fifth dimension,
`variant`, so two arms of an experiment can be read with the same delta
table, charts and CSV as any other comparison:

```text
GET /admin/api/compare?dimension=variant&key=3&a=control&b=candidate&window=all
```

```bash
modelrouter report compare --dimension variant --key 3 --a control --b candidate --window all
```

`key` is the experiment id; `a` and `b` are variant labels (`400 b:
experiment 3 has no variant x` otherwise). The response carries an extra
`experiment` block — `id`, `name`, `status`, `retain_content`,
`content_retention_days` and a `stored_content_note` saying whether the
prompt log holds the content behind the arms — and its quality caveat points
at the results page, where the reported outcomes live. The dashboard's
**Compare** page lists experiments in the key picker and their labels in
the arm pickers. Use `window=all`: an experiment's life is its window.

## 7. Lifecycle

- **Expiry.** A request after `expires_at` is refused (`has expired`) even
  before the server has noticed; the lifecycle tick then closes the
  experiment within 60 seconds, after which the same request says
  `is closed`. `expires_at: 0` never expires and must be closed by hand.
- **Closing** is the only state change and is final. Closed experiments
  keep their rows, results and comparisons; they just stop binding.
- **Scope.** `allowed_user_ids` restricts binding to those users' keys; an
  empty list admits every key. The check runs before the correlation-id
  check, so a user outside the list is told so rather than asked for an id.
- **Uniqueness.** Names are unique forever; close-and-recreate needs a new
  name. Ids are never reused.
- **Audit.** Create and close are audited with actor and full row.
- **Visibility.** REST and dashboard writes reload the live registry
  immediately. CLI writes are picked up by the next tick, at most 60 seconds
  later.

## 8. Content retention

By default the router stores prompt and response *content* only when
`[storage] store_prompt_content` says so, and deletes prompt rows older than
`prompt_retention_days`. An experiment can override that for its own
traffic — a reviewer often needs to read the answers, not just the ledger.

Retention is set at creation (so, like every create, by a **superadmin**)
with `retain_content: true`, and it requires a finite `expires_at`
(`retain_content: true requires expires_at to be set; an experiment that
never expires cannot retain content`): content that is kept must have a
known end. For every bound
request the router then writes a prompt row with full content regardless of
`store_prompt_content`, and the global retention sweep skips those rows
while the experiment's window is open. `x-no-log` still wins: a caller who
asks not to be logged is not logged, experiment or not. Retained content
never widens what leaves the router — callbacks (Langfuse, LangSmith,
webhooks) see exactly what they would without the experiment.

The window closes `content_retention_days` after the experiment closes
(`0` = never). When it does, an hourly tick redacts the content columns of
the experiment's prompt rows in place to the shape a non-retaining row has
— latency, tokens, ids and stamps survive, so results still read the same
— and clears the `note` on its runs' outcomes. The results document reports
`retained_content_bytes` while content is held; the dashboard badges a
retaining experiment with `retains content · <window>`; the compare page
says the same in `stored_content_note`. The **Close** confirmation spells
out what closing starts.

Where the content lives is the operator's concern: the prompt store is the
main database unless `[storage] prompt_db_path` points it elsewhere, and
encryption at rest is the deployment's job, not the router's. Nothing about
retention changes which admin roles can read prompt rows.

## 9. Reading the result honestly

- **Quality is the application's column.** Cost and latency say nothing
  about answers; `outcomes` is the only quality signal here and it is only
  as complete as the application's reporting. `success_rate` is `null`,
  not `0`, when nothing was reported.
- **`latency_samples` is not `requests`.** Latency comes from the prompt
  log, which is written only where storage or retention allows. When the two
  counts differ, latency describes a subset.
- **`unpriced`** on a variant means a model in it has no `[pricing]` entry —
  which the creation gate prevents, unless pricing was removed afterwards.
  Its cost is an undercount and is not recomputed when the price is added.
- **`estimated_rows > 0`** means streamed calls; their token and cost
  figures are estimates.
- **`mixed_runs`** and **`unbound_requests`** are the two ways a run can be
  less clean than it looks: a run bound to two variants, and turns of a run
  sent without the header. Both are counted and shown, never merged into a
  variant's figures.
- **Cache hits never appear.** A bound request bypasses the cache, so
  `saved_usd` is zero and no variant ever "wins" by being served from cache.
- **Compare arms that ran over the same period.** p95 latency moves with
  provider load and time of day; hash assignment interleaves the variants
  in time, which is the main reason to prefer it over naming variants per
  deployment.

---

# Part 2 — Comparing arms you label yourself

Use this when the application chooses the model per request itself, or to
compare things that are not experiments at all — two deployments, two
providers, two days. Nothing is created up front and nothing changes
routing; the router compares two label values over a time window.

### 1. Design the experiment

An experiment is two **arms**. Each arm is a set of requests that share one
label value. The router compares any two label values along one of five
**dimensions**:

| `dimension` | Arm is identified by | Use it when |
|---|---|---|
| `tag` | one attribution tag key's value (`key=arm`, `a=…`, `b=…`) | the app assigns arms itself — the normal case |
| `run` | the attribution `correlation_id` | comparing two batch runs or two deployments |
| `model` | the backing model the router actually called | comparing everything that hit model X against model Y |
| `provider` | the upstream provider | comparing the same model served by two providers |
| `variant` | a variant of a router-managed experiment (`key=<experiment id>`) | reading a Part 1 experiment through the compare table; see [§6](#6-compare-two-variants) |

Decide up front:

- **What varies.** The model (`openai/gpt-4o` vs `openai/gpt-4o-mini`), the
  provider (`anthropic/…` vs `vertex/…`), a prompt version, a temperature.
  The router measures the effect of whatever the two arms differ in; it does
  not know or care what that is.
- **How requests are assigned.** Do it client-side and deterministically —
  hash a stable id (user, session, order) so the same unit always lands in the
  same arm. Do not alternate per request if a session carries context between
  calls.
- **How long to run.** The compare window is the router's clock, not the
  experiment's: `daily`, `weekly` (last 7 days), `monthly` (since the first of
  the month, the default), or `all`. Pick an arm label that is unique to this
  experiment so `window=all` gives you exactly the experiment.

#### What the router can and cannot measure

The comparison reports, per arm: requests, total and per-request cost,
tokens in/out (total and per request), cache hits and hit rate, failures and
error rate, and mean / p50 / p95 latency. It cannot report:

- **Quality.** There is no quality column. A cheaper, faster arm is not a
  better arm; the application has to judge answers itself.
- **Time to first token.** Not recorded by the router today.
- **Anything about streamed responses that you can trust.** Streamed calls
  record estimated or zero tokens and, on the messages API, a placeholder
  latency, and those rows are indistinguishable from measured ones. **Send
  experiment traffic with `"stream": false`.**

### 2. Label the traffic

Every metered endpoint accepts an `attribution` block. Put the experiment
name and the arm in `tags`, and use `correlation_id` for the finer unit you
might want to compare later (a batch run, a deploy, a day):

```json
{
  "model": "openai/gpt-4o-mini",
  "stream": false,
  "messages": [{ "role": "user", "content": "…" }],
  "attribution": {
    "correlation_id": "checkout-v2:b:2026-09-02",
    "tags": { "experiment": "checkout-v2", "arm": "b" }
  }
}
```

With the OpenAI SDKs this goes through `extra_body` (Python) /
`extraBody` (TypeScript). If the SDK will not forward unknown body fields,
the header channel carries the same data:

```text
X-Attribution-Correlation-Id: checkout-v2:b:2026-09-02
X-Attribution-Tags: {"experiment":"checkout-v2","arm":"b"}
```

Rules that matter for an experiment:

- **Partition on a single tag key.** `dimension=tag` compares two values of
  *one* key. `key=arm&a=a&b=b` selects every request whose `arm` tag is `a`
  against every request whose `arm` tag is `b` — across all experiments. If
  you run several experiments at once, make the arm value unique to the
  experiment (`"arm": "checkout-v2:a"`) rather than relying on a second tag to
  disambiguate; the comparison does not intersect tags.
- **Use `provider/model` names.** A bare model name that is not a configured
  alias falls through to `routing.default_model`, so both arms would silently
  hit the same model. `openai/gpt-4o-mini`, `anthropic/claude-sonnet-5`,
  `vertex/…` are unambiguous. The `model` dimension compares the model the
  router actually called (`gpt-4o-mini`), not the name the client sent.
- **Attribution never changes routing, pricing, or the cache key.** Two arms
  that send the same prompt to the same model share one cache entry, and the
  cache hit is metered to whichever arm made the call that hit. If the arms
  differ only in a tag, the second one to send a prompt will look free — put
  the thing you are testing in the request, not just in the label.
- **Bounds** (a `400` on violation): correlation id ≤ 128 characters; at most
  8 tags; keys ≤ 64 characters from `[A-Za-z0-9_-.:]`; values ≤ 128
  characters; the encoded tag map ≤ 1 KB.

The router records the labels on the prompt log, the cost ledger, and the
failure log, so all three metric families can be sliced the same way.

### 3. Retrieve the comparison

#### Credentials

The comparison lives behind the admin API. Give the application its own
**viewer** admin — viewers can read every admin endpoint and mutate nothing:

```bash
modelrouter admin create --name checkout-svc --role viewer
# prompts for a password; store it in the app's secret store
```

Exchange the password for a JWT at start-up and keep it in memory:

```bash
curl -s -X POST http://router:8080/admin/api/login \
  -H 'Content-Type: application/json' \
  -d '{"name":"checkout-svc","password":"…"}'
# {"token":"eyJ…"}
```

The token expires after `auth.jwt_expiry_mins` (default 60). Treat a `401`
from any admin endpoint as "log in again", not as an error to surface.

#### `GET /admin/api/compare`

```text
GET /admin/api/compare?dimension=tag&key=arm&a=a&b=b&window=weekly
Authorization: Bearer <jwt>
```

| Parameter | Required | Values |
|---|---|---|
| `dimension` | yes | `model`, `provider`, `tag`, `run`, `variant` |
| `key` | for `tag` and `variant` | the attribution tag key to partition on, or the experiment id; ignored otherwise |
| `a`, `b` | yes | the two arm values; must differ; ≤ 256 characters |
| `window` | no | `daily`, `weekly`, `monthly` (default), `all` |

A malformed query returns `400` with a message naming the field
(`"key is required when dimension=tag"`, `"a and b must differ"`,
`"key must be an experiment id (a positive integer) when dimension=variant"`, …).
An arm with no rows is not an error: it comes back with zero requests and
`null` per-request figures.

The other dimensions take the same shape:

```text
GET /admin/api/compare?dimension=run&a=checkout-v2:b:2026-09-01&b=checkout-v2:b:2026-09-02
GET /admin/api/compare?dimension=model&a=gpt-4o&b=gpt-4o-mini&window=monthly
GET /admin/api/compare?dimension=provider&a=anthropic&b=vertex&window=all
GET /admin/api/compare?dimension=variant&key=3&a=control&b=candidate&window=all
```

#### The response

A comparison of the example above, abridged. Field order and names are
exactly what the endpoint returns; the CLI's `--format json` emits the same
document.

```jsonc
{
  "dimension": "tag",
  "key": "arm",
  "window": "weekly",
  "start": "2026-08-27T22:26:46Z",        // window bounds, UTC
  "end":   "2026-09-04T22:26:46Z",
  "a": {
    "value": "a",
    "label": "arm=a",
    "requests": 182,                       // rows in the cost ledger
    "cost_usd": 1.0061025,
    "cost_per_request": 0.005528,          // null when requests == 0
    "saved_usd": 0.06484,                  // what cache hits would have cost
    "tokens_in": 190633,
    "tokens_out": 59436,
    "tokens_in_per_request": 1047.43,
    "tokens_out_per_request": 326.57,
    "cache_hits": 11,
    "hit_rate": 0.0604,                    // cache_hits / requests
    "failures": 1,                         // rows in the failure log
    "error_rate": 0.00546,                 // failures / (requests + failures)
    "latency": {
      "samples": 182,                      // prompt-log rows with a measured latency
      "mean_ms": 1985.5,
      "p50_ms": 1953,                      // nearest-rank percentiles; null when samples == 0
      "p95_ms": 2525
    },
    "unpriced": false,                     // true if any model in the arm has no price
    "unpriced_models": [],
    "by_day": [
      { "key": "2026-09-01", "cost_usd": 0.339, "saved_usd": 0.019,
        "tokens_in": 62766, "tokens_out": 20119, "requests": 60, "cache_hits": 3 },
      …
    ]
  },
  "b": { "value": "b", "label": "arm=b", "requests": 182, "cost_usd": 0.0619, … },
  "delta": {                               // b − a; null when a metric is undefined on either side
    "requests":            { "abs": 0.0,      "pct": 0.0 },
    "cost_usd":            { "abs": -0.944,   "pct": -93.85 },
    "cost_per_request":    { "abs": -0.00519, "pct": -93.85 },
    "tokens_in":           { "abs": -2005.0,  "pct": -1.05 },
    "tokens_out":          { "abs": 6828.0,   "pct": 11.49 },
    "tokens_in_per_request":  { "abs": -11.0, "pct": -1.05 },
    "tokens_out_per_request": { "abs": 37.5,  "pct": 11.49 },
    "hit_rate":            { "abs": 0.033,    "pct": 54.5 },
    "error_rate":          { "abs": 0.0054,   "pct": 98.9 },
    "mean_ms":             { "abs": -934.0,   "pct": -47.0 },
    "p50_ms":              { "abs": -926.0,   "pct": -47.4 },
    "p95_ms":              { "abs": -1093.0,  "pct": -43.3 }
  },
  "coverage": {
    "a": { "requests": 182, "latency_samples": 182 },
    "b": { "requests": 182, "latency_samples": 182 },
    "incomplete_pairs": null               // reserved; always null today
  },
  "experiment": null,                       // set for dimension=variant only; see Part 1 §6
  "ttft": null,
  "ttft_note": "Time to first token is not recorded by the router today, so it cannot be compared.",
  "caveats": [
    "This comparison has no quality column. A difference in cost or latency is not evidence of a difference in answer quality.",
    "Streamed responses record estimated or zero tokens and, on the messages API, a placeholder latency; they are indistinguishable from measured rows here. Send experiment traffic with stream: false."
  ]
}
```

`pct` is `null` when the A-side value is zero (there is no percentage of
nothing), and a whole delta is `null` when either side has no value for that
metric — a per-request figure with zero requests, a percentile with zero
samples.

#### From the terminal

The same comparison, run directly against the database by the router's CLI
(no server, no JWT — it needs the config file and read access to the SQLite
file):

```bash
modelrouter report compare --dimension tag --key arm --a a --b b --window weekly
```

```text
Compare by tag: A = arm=a  B = arm=b  (window: weekly)
┌──────────────────────────────────┬────────┬────────┬─────────────┬────────┐
│ Metric                           ┆ A      ┆ B      ┆ Delta (B-A) ┆ Change │
╞══════════════════════════════════╪════════╪════════╪═════════════╪════════╡
│ Requests                         ┆ 182    ┆ 182    ┆ 0           ┆ 0.0%   │
│ Cost / request (USD)             ┆ 0.0055 ┆ 0.0003 ┆ -0.0052     ┆ -93.8% │
│ Tokens in / request              ┆ 1047.4 ┆ 1036.4 ┆ -11.0       ┆ -1.1%  │
│ Tokens out / request             ┆ 326.6  ┆ 364.1  ┆ +37.5       ┆ +11.5% │
│ Mean latency (ms, n=182 / n=182) ┆ 1985.5 ┆ 1051.5 ┆ -934.0      ┆ -47.0% │
│ p50 latency (ms)                 ┆ 1953   ┆ 1027   ┆ -926.0      ┆ -47.4% │
│ p95 latency (ms)                 ┆ 2525   ┆ 1432   ┆ -1093.0     ┆ -43.3% │
│ Cache hit rate                   ┆ 6.0%   ┆ 9.3%   ┆ +3.3%       ┆ +54.5% │
│ Error rate                       ┆ 0.5%   ┆ 1.1%   ┆ +0.5%       ┆ +98.9% │
│ Total cost (USD)                 ┆ 1.0061 ┆ 0.0619 ┆ -0.9442     ┆ -93.8% │
│ Total tokens in                  ┆ 190633 ┆ 188628 ┆ -2005       ┆ -1.1%  │
│ Total tokens out                 ┆ 59436  ┆ 66264  ┆ +6828       ┆ +11.5% │
│ Cache hits                       ┆ 11     ┆ 17     ┆ -           ┆ -      │
│ Failures                         ┆ 1      ┆ 2      ┆ -           ┆ -      │
└──────────────────────────────────┴────────┴────────┴─────────────┴────────┘
Coverage: A 182 latency samples of 182 requests; B 182 of 182.
Time to first token is not recorded by the router today, so it cannot be compared.
Note: This comparison has no quality column. …
Note: Streamed responses record estimated or zero tokens …
```

`--format csv` writes the table rows as CSV. The prose beneath the table is
not repeated there, but the data it summarises is: the `Latency samples` and
`Unpriced models` rows carry each arm's latency denominator and unpriced
models, so a spreadsheet sees the same caveats the table does. `--format json`
writes the endpoint document verbatim, so a script can consume either source
the same way. `--window alltime` is accepted as a synonym for `all`. An invalid query
prints the same message the endpoint would return and exits 1.

### 4. View it on the dashboard

`/admin/compare` (nav: **Compare**) is the same query behind a form: pick the
dimension, the two arm values (the pickers are populated from what the
ledger has actually seen), and the window. It renders the metric table with
the delta column, three charts — cost per request and requests by day per
arm, and latency percentiles side by side — and the coverage line under the
table. Two things to look for before believing a number:

- **The latency denominator is not the cost denominator.** "Mean latency
  (n=182 / n=182)" tells you how many prompt-log rows carried a latency; the
  request count comes from the cost ledger. When the prompt log is off, or
  stores fewer rows than the ledger, the latency figures describe a
  subset, and the page says so.
- **An "unpriced" badge on an arm** means at least one model in that arm is
  missing a price: either it has no entry in `[pricing]` now, or its ledger
  rows consumed tokens on real provider calls yet recorded zero spend, which
  is what a model looks like when it was priced only after the traffic ran.
  Either way every cost figure for that arm is an undercount. Adding the
  price does *not* recompute the ledger — fix pricing before the experiment,
  not after — and the badge keeps flagging the historical rows until they
  age out of the window.

### 5. Reading the result

- **Cost per request and tokens out per request** are the honest cost
  signal; total cost only matches if the arms saw equal traffic. In the
  example, arm B is 94 % cheaper per request while producing 11 % more
  output tokens — the saving is the price, not the verbosity.
- **p95 latency** moves for reasons other than the model (provider load,
  time of day). Compare arms that ran over the same period; `dimension=run`
  with one correlation id per day is a cheap way to check whether a
  difference is stable.
- **Error rate** counts every request the router recorded as failed — at
  any stage: resolution, policy, the upstream, a malformed request — against
  completed ones; `/admin/failures` breaks the count down by stage. A small
  absolute count with a large percentage change — `+98.9 %` on 1 vs 2
  failures above — is noise; look at the count.
- **Cache hit rate** differing between arms usually means the arms are not
  seeing equivalent prompts, not that one model caches better. Cache keys do
  not include the label.
- Nothing here says which arm gave better answers. Pair the comparison with
  whatever quality signal the application has — human review, an automated
  grader, task completion — before deciding.
