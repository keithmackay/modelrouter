# Critical Design Review: 2026-09-02-intelligent-model-routing-design (Round 1)

**Spec:** `/home/Laird.Popkin/src/modelrouter/docs/superpowers/specs/2026-09-02-intelligent-model-routing-design.md`
**Verified Assumptions section:** MISSING

> ⚠️ This spec lacks a `Verified assumptions` section. Reviewer cannot distinguish verified facts from unverified assumptions; treat findings accordingly.

## 0. Coverage enumeration

**Sections**

| Row | Disposition |
|---|---|
| §1 Goal | ok — states outcome; three pillars consistent with §4/§7/§7a |
| §2 Current state | ok — claims re-verified against code: seam at `completions.rs:79` (`maybe_downgrade` call present, `user: AuthenticatedUser` in scope at that point); ollama via OpenAI-compat confirmed (`config.example.toml` `[providers.ollama]`); no per-provider concurrency (only per-user `ConcurrencyLimiter`); `/admin/api/models/available` exists (PR #34) |
| §3 Decisions table | ok — matches body; "per-user/per-key" decision row vs user-only data model → §2.2 |
| §4 Architecture steps 1–6 | step 4 "unchanged downstream pipeline" → §2.1 (policy allow-list can deny the rerouted model); rest ok |
| §4a ProviderCapacity | ok — new component; `ProviderConfig` (schema.rs:527) has no `deny_unknown_fields`, new fields addable |
| §5 DB tables | `user_routing_assignments` keyed by `user_id` only → §2.2 |
| §5 Config | ok — capacity fields under `[providers.*]` addable |
| §5 API/CLI | ok — mirrors webhook conventions in CLAUDE.md |
| §6 Classification | classifier default contradicts §5 → §2.5; capacity-fallback heuristic mapping undefined → §3.1 |
| §7 Adaptive allocation | "cheapest member" rests on pricing data → §2.3 |
| §7a Experiments | session-affinity mechanism claim → §2.4 |
| §8 Error handling table | classifier-at-capacity row unimplementable as written → §3.1 (same item) |
| §9 Observability | ok — dimensions traceable to `routing_decisions`/`model_quality_stats` columns; "dollars saved" reference price is a display metric, dropped — not literal wrongness |
| §10 Testing | ok — pure-function selection testable; seedable RNG stated |
| §11 Phasing | ok — Phase 2 cleanly additive over Phase 1 tables |
| §12 Out of scope | ok — consistent with body |

**Rules and operands**

| Row | Disposition |
|---|---|
| Policy trigger predicate (`all` requests from assigned users) — producer sweep of request entry points | under-inclusion: `/v1/responses` has no smart-routing seam (grep `complexity_router\|maybe_downgrade` in `src/api/routes/responses.rs`: no hits) → §2.6 |
| Tier/member skip rule (breaker, disabled, throttle, cap) | ok — all four states exist or are defined by the spec (`CircuitBreaker`, `AvailabilityMap`, new ProviderCapacity); over-skip → next tier, under-skip → normal dispatch failure path |
| Exploit rule ("cheapest member with score ≥ threshold") — operand: member cost | cost of unpriced model is silently `0.0` (`src/router/cost.rs:228` `None => 0.0`) → §2.3 |
| Explore rule ("cheapest under-sampled member at most one tier below") | ok — direction unambiguous in context ("next-cheaper", "bounding quality risk"); dropped as candidate |
| Family/prefix heuristic for auto-trial — identity rule, over-merge check against real catalog names | over-merges distinct families (`gpt-4o` prefix-matches `gpt-4o-mini`, a different cost class) → §2.7 |
| Variant assignment (stable hash of `session_id`) | ok — deterministic per session; sessions without `session_id` must carry the explicit variant tag (covered by §2.4 fix) |
| Session classification cache key (`session_id`) | ok — absent session_id → classify per request; degraded but correct |
| Feedback rating (1–5, keyed by decision id) | ok — decision row carries category/provider/model needed for the stats update; 30-day TTL bounds late feedback, aggregates survive pruning (stated in spec) |

**Data-flow arrows**

| Row | Disposition |
|---|---|
| Classifier JSON → tier selection | ok — parse-failure and timeout paths defined (§6, §8) |
| Smart choice → downstream `policy.check(&user, &model)` | crosses into an authorization operation whose operand is the *rerouted* model → §2.1 |
| Decision id → `x-modelrouter-decision` header → `POST /v1/feedback` → decision row → stats | ok — id minted pre-dispatch, header set before body on both streaming and non-streaming; feedback row → stats cell fields all present in §5 schema |
| Response content → async judge task | ok — in-memory handoff at response time; nothing needed from persisted artifacts |
| Provider catalog → trial enrollment | mechanism is the family heuristic → §2.7 (same defect, one row per surface) |
| `routing_decisions` (persisted) → comparison/analysis views | ok — every §9 dimension maps to a named §5 column (outcome, tokens, cost, latency/TTFT, judge, rating) |
| Ladder → escalation-on-failure candidates (replacing `fallback_chains`) | ok — non-streaming only, consistent with existing behavior; streaming decided pre-dispatch as spec states |

## 1. Verified-assumptions cross-check

Omitted — spec has no `Verified assumptions` section (warning above).

## 2. Literal-wrongness findings

**2.1 The downstream policy check can 403 the model smart routing chose, violating the spec's own "never less servable" guarantee.**
Evidence: spec §4 step 4 sends the chosen member through the "unchanged downstream pipeline (budget policy check …)"; `PolicyEngine::check` denies when a budget rule has a non-empty `allow_models` that doesn't contain the model (`src/router/policy.rs:79`). A user whose rule allows `openai/gpt-4o` requests it, smart routing reroutes to `ollama/llama3`, policy denies → 403 where today the request succeeds. Directly contradicts §4 step 3: "A policy can never make a request less servable than today."
Proposed fix: member selection filters candidates through the user's allow/deny rules (the same check `PolicyEngine` applies, evaluated per candidate); if every ladder member is filtered, route as if no policy existed (the existing all-tiers-exhausted path).

**2.2 Per-token learning opt-in is not representable in the proposed data model.**
Evidence: the asked-for behavior is "a specific option that users could turn on for a token"; §5 keys assignments by `user_id` only. Keys are a separate table with many-per-user semantics: `migrations/002_per_key_budgets.sql` (`api_keys` table), `AuthenticatedUser.api_key_id: Option<i64>` (`src/db/models.rs:13`). A user with two tokens (the pilot application + something else) cannot enable learning on one.
Proposed fix: `user_routing_assignments` gains nullable `api_key_id`; a key-scoped assignment overrides a user-scoped one; `learning_enabled` is honored at whichever scope matched. Note `api_key_id` is `None` for legacy key auth (models.rs:11) — legacy-auth callers can only be user-scoped.

**2.3 Unpriced models cost $0, corrupting both "cheapest member" selection and the trial cost comparison the feature exists to answer.**
Evidence: `CostCalculator` returns `0.0` for any model without a pricing entry (`src/router/cost.rs:228`, `None => 0.0`). The exploit rule (§7) picks "the cheapest member"; a newly enrolled trial (`fable-5-1`, the spec's own motivating example) has no pricing entry on day one, so it ties with ollama at $0 and the head-to-head comparison reports the new model as free — "is 5.1 cheaper than 5.0?" gets a literally wrong answer.
Proposed fix: distinguish "free by declaration" from "unknown": cost lookup returns `Option`; ladder members and auto-trial enrollees without a pricing entry (and not marked `free = true` on the provider) are ranked after priced members in cheapest-first ordering, and the comparison view shows "cost: no data" instead of $0. Policy validation warns on unpriced members.

**2.4 §7a claims session affinity keeps an engagement on its variant; the affinity primitive does not do that.**
Evidence: `resolve_with_pin` returns the *newly resolved* provider/model in every divergence case — same provider/different model → resolved model (`src/router/session_affinity.rs:100-108`), different provider → pin cleared, resolved used (`:111-117`). It never holds a session on a previous choice, and same-provider variants (fable-5-0 vs 5-1, both `anthropic`) are invisible to it anyway. If a client sends the experiment header only on the first request of an engagement, nothing keeps subsequent turns on the variant.
Proposed fix: delete the affinity claim. Variant stickiness comes from the two mechanisms that are actually deterministic: the explicit `<experiment>:<variant>` tag sent on every request of a run, or router assignment by stable session-id hash (which yields the same variant every turn without any pinning). Document that header-tagged runs must tag every request.

**2.5 Classifier default is specified two contradictory ways.**
Evidence: §5 — `classifier_model` "(defaults to global setting…)"; §6 — "Classifier defaults to the cheapest tier-1 member." A policy with no classifier and tier 1 = `[ollama/llama3]` under global `classifier_model = "openai/gpt-4o-mini"` routes classification to two different models depending on which sentence the implementer reads.
Proposed fix: one precedence chain: `policy.classifier_model` → `[smart_routing].classifier_model` → reject policy at creation time if neither is set. Keep "point it at a local model so classification is free" as guidance, not as a default rule.

**2.6 The `all` trigger does not cover all requests: `/v1/responses` has no smart-routing seam.**
Evidence: §4 step 1 defines the trigger as "`all` requests from assigned users"; the architecture wires SmartRouter only into `chat_completions_inner`. `src/api/routes/responses.rs` contains no `complexity_router`/`maybe_downgrade` call site (grep: zero hits) — assigned users' `/v1/responses` traffic silently bypasses the policy, so the spec's stated trigger semantics are false as written.
Proposed fix: pick and state the scope. Either (a) Phase 1 scopes smart routing to `/v1/chat/completions` and the spec says so explicitly (rename trigger semantics to "all chat-completion requests"), or (b) add the same pre-routing seam to `/v1/responses`. (a) is the smaller, honest change; (b) is required if the pilot application drives engagements through `/v1/responses`.

**2.7 The auto-trial family heuristic over-merges distinct model families on real catalog names.**
Evidence: §7 — "new models in the same family as an existing member (prefix/family heuristic, config-overridable)". Prefix matching conflates families that differ only by suffix on real catalogs: `gpt-4o-mini` is a prefix-extension of `gpt-4o` but a different capability/cost class; ollama tags (`llama3` → `llama3.1`, `llama3:70b`) behave the same way. Identity-rule mechanics can't be repaired by calibration: at every threshold, a wrong-family model gets enrolled, receives explore traffic, and pollutes the incumbent's head-to-head report.
Proposed fix: no default heuristic. `auto_trial` enrollment requires an explicit per-member match pattern (e.g. `trial_match = "claude-fable-*"` on the ladder member); members without a pattern are never auto-paired. The catalog scan matches new names against declared patterns only.

## 3. Forced decisions

**3.1 The classifier-at-capacity fallback names a heuristic with no decision rule.**
The choice: §6/§8 say "skip the LLM call and use the token-count heuristic (`estimate_tokens_from_messages`) for that request" — but a token estimate is a number, and the spec defines no mapping from that number to a tier, so the row is unimplementable until one is picked.
Why it's forced: `estimate_tokens_from_messages` (`src/router/complexity.rs`) produces an estimate; the existing `ComplexityRouter` compares it to a single threshold to pick one fixed model — neither the threshold nor the target generalizes to an N-tier ladder without a decision.
Options: (a) per-policy token thresholds mapping estimate → tier (new config, more knobs); (b) drop the heuristic entirely and use the policy's `default_tier`, same as the classifier-failure row (simpler, one consistent degraded path); (c) tier 1 always (maximally cheap, accepts quality risk during classifier outages).

## 5. Recommendation

🛑 **Surface forced decisions to user** — §3.1 needs a pick, and the seven §2 items need fixes (none require re-architecting; 2.1–2.5 and 2.7 are spec-text/data-model amendments, 2.6 is a scope statement or one added seam).
