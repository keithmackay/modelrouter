# Running a model experiment through modelrouter

This guide is for a client application that wants to answer a question like
"is `gpt-4o-mini` good enough for our checkout assistant, and what does it
save?" — by sending real traffic down two arms and reading the difference
back from the router. It covers the whole loop: how to label traffic, how to
retrieve the comparison as JSON or from the terminal, and how to read it on
the dashboard.

The router does not assign arms, pick models, or judge answers. It records
what each request cost, how long it took, and whether it failed, keyed by the
labels the caller attaches. The application owns the experiment design; the
router owns the measurement.

## 1. Design the experiment

An experiment is two **arms**. Each arm is a set of requests that share one
label value. The router compares any two label values along one of four
**dimensions**:

| `dimension` | Arm is identified by | Use it when |
|---|---|---|
| `tag` | one attribution tag key's value (`key=arm`, `a=…`, `b=…`) | the app assigns arms itself — the normal case |
| `run` | the attribution `correlation_id` | comparing two batch runs or two deployments |
| `model` | the backing model the router actually called | comparing everything that hit model X against model Y |
| `provider` | the upstream provider | comparing the same model served by two providers |

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

### What the router can and cannot measure

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

## 2. Label the traffic

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

## 3. Retrieve the comparison

### Credentials

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

### `GET /admin/api/compare`

```text
GET /admin/api/compare?dimension=tag&key=arm&a=a&b=b&window=weekly
Authorization: Bearer <jwt>
```

| Parameter | Required | Values |
|---|---|---|
| `dimension` | yes | `model`, `provider`, `tag`, `run` |
| `key` | for `tag` only | the attribution tag key to partition on; ignored otherwise |
| `a`, `b` | yes | the two arm values; must differ |
| `window` | no | `daily`, `weekly`, `monthly` (default), `all` |

A malformed query returns `400` with a message naming the field
(`"key is required when dimension=tag"`, `"a and b must differ"`, …).
An arm with no rows is not an error: it comes back with zero requests and
`null` per-request figures.

The other dimensions take the same shape:

```text
GET /admin/api/compare?dimension=run&a=checkout-v2:b:2026-09-01&b=checkout-v2:b:2026-09-02
GET /admin/api/compare?dimension=model&a=gpt-4o&b=gpt-4o-mini&window=monthly
GET /admin/api/compare?dimension=provider&a=anthropic&b=vertex&window=all
```

### The response

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

### From the terminal

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
│ Requests                         ┆ 182    ┆ 182    ┆ 0           ┆ +0.0%  │
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

`--format csv` writes the table rows as CSV; `--format json` writes the
endpoint document verbatim, so a script can consume either source the same
way. `--window alltime` is accepted as a synonym for `all`. An invalid query
prints the same message the endpoint would return and exits 1.

## 4. View it on the dashboard

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
- **An "unpriced" badge on an arm** means at least one model in that arm has
  no entry in `[pricing]`, so its cost is recorded as zero and every cost
  figure for that arm is an undercount. Add the price and the ledger rows
  are *not* recomputed — fix pricing before the experiment, not after.

## 5. Reading the result

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
