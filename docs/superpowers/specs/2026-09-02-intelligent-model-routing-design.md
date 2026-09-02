# Intelligent Model Routing — Design

**Date:** 2026-09-02
**Status:** Approved design, pre-implementation

## 1. Goal

Maximize use of free/cheap models (e.g. a local ollama instance, free-tier SaaS
models) and spend on paid models only when a task exceeds the cheaper models'
ability or their capacity. Three pillars:

1. **Difficulty-aware routing** — an LLM classifier rates each request and the
   router picks the cheapest tier rated capable.
2. **Tiered pools with capacity overflow** — each tier is a pool of
   provider/model members; the router works through a tier's members (skipping
   throttled, at-capacity, or unhealthy ones) before spilling to the next,
   more expensive tier.
3. **Adaptive allocation (opt-in, Phase 2)** — enabled per application API
   key (the modelrouter key an app authenticates with — not an LLM token):
   the router learns per prompt-category which ladder member is the cheapest
   one that is good enough, and automatically trials newly released model
   versions against incumbents.

Pilot for Phase 2: the Athena deployment's API key gets `learning_enabled`,
so its traffic trains the quality stats while all other consumers stay on
deterministic routing.

**Scope:** Phase 1 smart routing applies to `POST /v1/chat/completions`
only. `/v1/responses` (and embeddings/images) bypass routing policies; a
`/v1/responses` seam is a listed future extension (§12).

## 2. Current state (relevant seams)

- Chat completions flow through `chat_completions_inner`
  (`src/api/routes/completions.rs`). `ComplexityRouter::maybe_downgrade` runs
  before policy/resolution/dispatch — that pre-routing seam is where smart
  routing slots in.
- Ollama is already served by the OpenAI-compat adapter via
  `[providers.ollama]`; no new adapter is needed.
- `CircuitBreaker` (per provider), `FallbackChain`, `LoadBalancer` pools, and
  operator availability gates exist downstream and are unchanged.
- Concurrency limiting today is per **user** only; there is no per-provider
  capacity tracking. The streaming path has no retry/fallback, so routing
  decisions must happen pre-dispatch (they do, in this design).
- `/admin/api/models/available` aggregates provider catalogs (TTL-cached) —
  used by new-model trial detection.

## 2a. Verified assumptions

Checked against the codebase on 2026-09-02:

- The pre-routing seam has the authenticated user in scope:
  `chat_completions_inner(State, user: AuthenticatedUser, …)` and the
  `maybe_downgrade` call both live at the top of the handler
  (`src/api/routes/completions.rs:57-80`).
- API keys are many-per-user: `api_keys` table
  (`migrations/002_per_key_budgets.sql`),
  `AuthenticatedUser.api_key_id: Option<i64>` (`src/db/models.rs:13`,
  `None` for legacy key auth).
- Unknown models cost $0 in the current calculator
  (`src/router/cost.rs:228`, `None => 0.0`) — hence the pricing
  discipline in §7.
- Session affinity always defers to the newly resolved provider/model
  (`src/router/session_affinity.rs:94-117`); it cannot hold a session on
  an experiment variant — hence §7a's tag-every-request rule.
- `/v1/responses` has no pre-routing seam (no `complexity_router` call in
  `src/api/routes/responses.rs`) — hence the Phase 1 scope statement in §1.
- Budget rules can deny by model allow-list (`src/router/policy.rs:79`) —
  hence the per-candidate policy filter in §4 step 3.
- `ProviderConfig` (`src/config/schema.rs:527`) accepts new optional fields
  (no `deny_unknown_fields`).
- `/admin/api/models/available` (aggregated, TTL-cached provider catalogs)
  exists — commit 2971f2b (#34).

## 3. Decisions (from brainstorming)

| Question | Decision |
|---|---|
| How to judge task difficulty | LLM-based classifier (strict-JSON, small prompt) |
| How to detect local capacity | Per-provider/model concurrency cap (no queueing); 429s add a throttle cooldown |
| Opt-in mechanism | Per-user/per-key routing policy assignment (DB), following the budget-rules pattern |
| Tier shape | Ordered ladder of N tiers; each tier is a pool of provider/model members |
| Quality signals for learning | All three: implicit failure signals (always on), LLM-as-judge sampling, client feedback API. Weight: feedback > judge > implicit |
| Learning state space | Bounded: policy ladder members (~5–15) × fixed 8-category taxonomy; request parameters excluded |
| New model versions | Auto-enrolled as trial candidates when `auto_trial` is set; manual promotion |
| Rollout | Phase 1 deterministic routing; Phase 2 learning, per-application-API-key opt-in, off by default |

## 4. Architecture

New component `SmartRouter` runs at the existing pre-routing seam (where
`ComplexityRouter` sits today). Per request:

1. **Policy lookup.** If the requesting user/key has no routing policy
   assignment, the request proceeds exactly as today — zero behavior change.
   A policy declares its trigger: `all` chat-completion requests from
   assigned users/keys, or only requests naming the policy's `virtual_model`
   (e.g. `"auto"`).
2. **Classify.** A compact prompt (system prompt + truncated conversation
   tail) goes to the policy's classifier model. Response is strict JSON:
   `{"difficulty_tier": <1..N>, "category": <taxonomy>}`. The result is
   cached per session; re-classified when the conversation grows past a size
   threshold or after a prior escalation in that session.
3. **Select tier and member.** Starting at the classified tier, walk that
   tier's members per the tier's strategy (`ordered` or `round_robin`).
   Skip a member if its provider breaker is open, it is operator-disabled,
   it is in throttle cooldown, it is at its concurrency cap, **or the
   requesting user's budget/policy rules would deny it** (the same
   allow/deny check `PolicyEngine` applies downstream, evaluated per
   candidate — otherwise smart routing could reroute onto a model the
   user's `allow_models` rule 403s). Tier exhausted → next tier up. All
   tiers exhausted → route as if no policy existed (log warning, bump
   metric). A policy can never make a request less servable than today.
4. **Dispatch.** The chosen provider/model enters the unchanged downstream
   pipeline (budget policy check, session affinity, breaker, retry). Because
   selection is pre-dispatch, streaming and non-streaming behave identically.
5. **Escalation on terminal failure** (non-streaming): fallback candidates
   come from the remaining ladder (next members/tiers) instead of the static
   `fallback_chains`, preserving cost ordering.
6. **Record.** Every smart-routed request writes a `routing_decisions` row
   (from Phase 1 onward) capturing the classification, chosen member, and
   implicit outcome signals — the substrate for Phase 2 learning.

### New supporting component: `ProviderCapacity`

Per provider/model in-flight counter with a configured cap, plus a throttle
state fed by 429 responses (member sidelined for `throttle_cooldown_secs`).
This makes "local model busy" and "free tier throttled" visible to routing;
neither exists today.

## 5. Data model and configuration

### DB tables (new migrations)

- `routing_policies` — `id`, `name`, `enabled`, `trigger` (`all` |
  `virtual_model`), `virtual_model_name`, `classifier_model`,
  `default_tier` (used on classifier failure; default = top tier),
  `tiers` (JSON: `[{strategy, members: [{provider, model, trial,
  trial_match}]}]` — `trial_match` per §7 new-model trials),
  `explore_rate` (default 0.10), `judge_model`, `judge_sample_rate`
  (default 0.05), `quality_threshold` (default 0.7), `min_samples`
  (default 20), `auto_trial` (bool).
- `user_routing_assignments` — `user_id`, `api_key_id` (nullable),
  `policy_id`, `learning_enabled` (bool, default false). Scope: an
  assignment with `api_key_id` applies to that application API key only and
  overrides any user-scoped (`api_key_id IS NULL`) assignment; this is how
  learning is enabled for a single app key (e.g. Athena's) without
  affecting the user's other keys. Legacy-auth callers have no
  `api_key_id` (`AuthenticatedUser.api_key_id: Option<i64>`) and can only
  match user-scoped assignments.
- `model_quality_stats` — `policy_id`, `category`, `provider`, `model`,
  `samples`, `updated_at`, plus **each measured dimension kept separately**
  (never only the blend): `success_rate` (implicit signals),
  `avg_judge_score`, `avg_user_rating`, `rating_count`,
  `avg_cost_usd_per_query`, `avg_input_tokens`, `avg_output_tokens`,
  `avg_latency_ms`, `avg_ttft_ms`, and the derived `ewma_score` (the blended
  routing signal). The blend drives routing; the raw dimensions drive
  analysis.
- `routing_decisions` — the per-query measurement record, one row per
  smart-routed request: `id` (returned to client in response header),
  `request_ts`, `user_id`, `policy_id`, `category`, `classified_tier`,
  `chosen_provider`, `chosen_model`, `was_exploration`, `overflow_reason`
  (null | capacity | throttle | breaker), `outcome` (success |
  failure_kind: refusal, truncation, tool_call_invalid, provider_error,
  timeout), `retries`, `input_tokens`, `output_tokens`, `cost_usd`,
  `latency_ms`, `ttft_ms` (streaming), `judge_score` (nullable),
  `user_rating` (nullable, 1–5). Pruned on a TTL (config, default 30 days);
  aggregates in `model_quality_stats` survive pruning.

### Config (`config.toml`)

```toml
[smart_routing]
enabled = true
classifier_model = "ollama/llama3"     # global default
decision_log_ttl_days = 30

[providers.ollama]
# existing fields...
max_concurrent = 2                      # ProviderCapacity cap
throttle_cooldown_secs = 30             # sideline duration after a 429
free = true                             # genuinely $0 — distinct from "no pricing entry"
```

Capacity lives in config (a property of infrastructure); policies live in the
DB (an operator decision), consistent with existing patterns.

### API and CLI surface

- Admin REST: CRUD at `/admin/api/routing/policies`, assign/unassign at
  `/admin/api/routing/policies/:id/assign`, stats/comparison views at
  `/admin/api/routing/stats`. Admin JWT for reads, superadmin for writes,
  matching webhook endpoint conventions.
- Client: `POST /v1/feedback` `{decision_id, rating}` with `rating` an
  integer 1–5 (Bearer auth; the decision id arrives in the
  `x-modelrouter-decision` response header). User ratings are stored on the
  decision row and aggregated per (category, model) as their own dimension.
- CLI: `modelrouter routing policy add|list|delete|assign`,
  `modelrouter routing stats`.

## 6. Classification

- Fixed 8-category taxonomy: `code`, `writing`, `summarization`,
  `extraction`, `qa`, `chat`, `reasoning`, `other`. Coarse on purpose — it
  bounds the learning state space (~8 × ladder size cells per policy). Finer
  taxonomies are a future refinement.
- Classifier call has a 2s budget. Timeout, JSON parse failure, or the
  classifier's provider being at capacity all degrade the same way: route
  to the policy's `default_tier` (quality over cost) — one consistent
  degraded path.
- Classifier precedence: `policy.classifier_model` →
  `[smart_routing].classifier_model`; policy creation is rejected if
  neither is set. (Operators should point it at a local model so
  classification is free — guidance, not a default rule.) Classifier and
  judge token usage is recorded in the cost ledger, attributed to the
  request and flagged as routing overhead, so savings reports stay honest.

## 7. Adaptive allocation (Phase 2)

Active only when the requesting application API key's matched assignment
has `learning_enabled = true`.

- **Exploit:** for the classified category, choose the cheapest ladder member
  with `ewma_score ≥ quality_threshold` (default 0.7) and `samples ≥
  min_samples` (default 20). If no member qualifies, fall back to the
  classifier's tier choice — graceful degradation to Phase 1 behavior.
- **Pricing discipline:** "cheapest" never treats unknown pricing as free.
  The cost calculator currently returns $0 for models without a pricing
  entry (`src/router/cost.rs`, `None => 0.0`); for routing and comparison
  purposes an unpriced member instead gets the **average per-token price of
  all priced members in the same ladder** as its estimate, an **alert** is
  raised (policy-validation warning at save time plus a runtime admin alert
  and metric) telling the operator a `[[pricing]]` entry is needed, and
  every view showing its cost labels it "estimated — pricing entry
  missing". A provider can be declared `free = true` (e.g. ollama) to mean
  genuinely $0. The accounting ledger keeps recording actuals per the
  existing calculator; the average-price substitute applies to routing
  decisions and comparison displays, so a just-released model (e.g.
  `fable-5-1`) is never mistaken for free.
- **Explore:** with probability `explore_rate`, route to the cheapest
  under-sampled member **at most one tier below** the classified tier,
  bounding the quality risk of experiments.
- **Signals → EWMA:** implicit failures (refusal patterns, truncation,
  tool-call validation errors, terminal errors, retries) recorded inline;
  judge scoring runs as a non-blocking async task on `judge_sample_rate` of
  responses; feedback API updates retroactively via the decision row.
  Weights: feedback > judge > implicit.
- **Change over time:** sample counts decay slowly, so members unused for
  weeks drift back to "under-sampled" and get re-explored. Nothing is
  permanently trusted or condemned.
- **New-model trials:** with `auto_trial`, the router scans the aggregated
  provider catalog for new models matching a ladder member's **explicit
  trial pattern** (`trial_match`, a glob on the member, e.g.
  `"claude-fable-*"`) and enrolls them as trial members of that tier
  (`trial: true`). There is no default prefix heuristic — prefix matching
  over-merges distinct families on real catalogs (`gpt-4o` would match
  `gpt-4o-mini`, a different cost class); members without a `trial_match`
  are never auto-paired. Trials start unsampled, so
  exploration feeds them traffic automatically. The stats view renders a
  head-to-head comparison between trial and incumbent, per category, across
  **all measured dimensions**: success rate, user rating, judge score, token
  cost per query, and latency/TTFT — answering "fable 5.1 just shipped; is
  it better/faster/cheaper than 5.0?" with data rather than a single opaque
  score. **Promotion is manual**: an admin reviews the comparison and edits
  the ladder. Auto-promotion is a future extension.

## 7a. Controlled experiments (A/B runs)

Motivating case: Athena runs one engagement twice in parallel — once per
model — and compares outcomes. The client orchestrates the parallel runs;
modelrouter provides variant pinning, measurement grouping, and the
comparison report.

- `experiments` table: `id`, `name`, `variants` (JSON: label →
  `{provider, model}`), `status` (active | closed), `created_at`. Managed
  via `/admin/api/routing/experiments` and
  `modelrouter routing experiment add|list|close`.
- **Variant pinning:** a request carrying
  `x-modelrouter-experiment: <experiment_id>:<variant>` routes directly to
  that variant's model — classifier and ladder are bypassed, because the
  experiment is the routing decision. All measurements are still recorded;
  `routing_decisions` gains nullable `experiment_id` and `variant` columns.
  Header-tagged runs must send the tag on **every request** of the run —
  the existing session-affinity primitive cannot hold a session on a
  variant (it always defers to the newly resolved provider/model, and
  same-provider variants are invisible to it).
- **Router-assigned variants:** if the header carries only the experiment id,
  the router assigns a variant by stable hash of `session_id` — automatic
  50/50 splits without client-side assignment logic, and deterministic per
  session on every turn (no pinning needed). Requests without a
  `session_id` must use the explicit `<experiment>:<variant>` form.
- **Comparison:** the experiment report shows variants side by side per
  category across all measured dimensions (success rate, user rating, judge
  score, token cost per query, latency/TTFT) — the same report component as
  the auto-trial head-to-head, with an experiment filter instead of a
  trial-vs-incumbent filter.
- Experiment traffic is excluded from `model_quality_stats` updates by
  default (a deliberately forced route is not evidence about the ladder);
  an experiment can opt in with `feed_learning = true`.

## 8. Error handling

Principle: smart routing degrades, never breaks.

| Failure | Behavior |
|---|---|
| Classifier timeout / bad JSON | Route to policy `default_tier`; metric + log |
| Classifier provider at capacity | Route to policy `default_tier` (same degraded path); metric + log |
| All ladder members unavailable | Route as if no policy; warning + metric |
| Stats/decision-log write fails | Routing proceeds; learning skips the sample |
| Judge task fails | Silent; sample lost; no retry |

## 9. Observability

- Metrics: decisions per tier, selections per member, overflow events by
  reason (capacity / throttle / breaker), classifier latency and failure
  rate, actual explore rate, judge score distribution.
- `/admin/stats` gains a per-policy view: requests kept local vs escalated
  and estimated dollars saved (paid-tier price of the request minus actual
  cost) — the headline number for the feature.
- Per (category, model) analysis view backed by `model_quality_stats`,
  showing each dimension separately: success/failure rate, average user
  rating (with rating count), average judge score, token cost per query,
  and latency/TTFT. All routing conclusions must be traceable to these
  measurements — the blended `ewma_score` is a routing convenience, never
  the only number an operator can see.
- Every smart-routed response carries `x-modelrouter-decision`
  (decision id, category, tier, member, reason) for client-side debugging
  and for the feedback API.

## 10. Testing

- Unit: tier/member selection as a pure function of (policy, classification,
  capacity/breaker/throttle snapshots); classifier JSON parsing incl.
  malformed output; EWMA and decay math; seedable RNG for deterministic
  explore-path tests.
- Integration (mock providers): overflow at concurrency cap; throttle
  cooldown and recovery; classifier-down degradation; ladder-exhausted
  fallback; the no-policy path remains byte-identical to today; feedback
  round-trip via decision id.
- Migration tests for the new tables.

## 11. Phasing

- **Phase 1 — deterministic smart routing:** SmartRouter, classifier, tiered
  pools, ProviderCapacity, policy tables + admin/CLI surface, decision log
  (recording only), metrics. Delivers the core goal with fully predictable
  behavior.
- **Phase 2 — adaptive allocation & experiments:** explore/exploit, judge
  sampling, feedback endpoint, decay, auto-trial + comparison view, and
  controlled A/B experiments (§7a). Off by default, enabled per application
  API key, piloted on Athena's key. Disabling it reverts cleanly to Phase 1
  behavior.

## 12. Out of scope / future

- Smart routing on `/v1/responses` (and embeddings/images) — Phase 1 covers
  `/v1/chat/completions` only.
- Auto-promotion of trial models into ladders.
- Learning over request parameters (temperature etc.) — model choice only.
- Finer prompt taxonomies; per-tenant taxonomies.
- Latency-based capacity signals (cap + cooldown only, for now).
- Queueing for a local slot ("cap + short queue" was considered and
  rejected in favor of immediate overflow).
