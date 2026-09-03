# Critical Design Review: 2026-09-02-intelligent-model-routing-design (Round 2)

**Spec:** `docs/superpowers/specs/2026-09-02-intelligent-model-routing-design.md`
**Verified Assumptions section:** PRESENT (§2a)
**Prior round:** `2026-09-02-intelligent-model-routing-design-critical-review-1.md`

Round 2 reviews the revised spec for consistency with the modelrouter
architecture and operator UX. Round 1 examined internal coherence; this round
asks whether the design fits the system it lands in.

## 0. Round-1 disposition

| R1 finding | Status in current text |
|---|---|
| 2.1 downstream policy can 403 the rerouted model | Addressed — §4 step 3 filters candidates through the user's allow/deny rules |
| 2.2 per-key learning opt-in unrepresentable | Addressed — `user_routing_assignments.api_key_id`, key scope overrides user scope |
| 2.3 unpriced models cost $0 | Addressed **differently** — spec substitutes the ladder-average price rather than ranking unpriced members last. Carries its own defect → §2.3 below |
| 2.4 session affinity cannot pin a variant | Addressed — claim deleted, tag-every-request rule stated in §7a |
| 2.5 contradictory classifier default | Addressed — single precedence chain in §6 |
| 2.6 `all` trigger misses `/v1/responses` | Addressed — option (a), scope stated in §1 |
| 2.7 prefix heuristic over-merges families | Addressed — explicit `trial_match` glob, no default heuristic |
| 3.1 classifier-at-capacity undefined | Addressed — option (b), same `default_tier` path as every classifier failure |

The `Verified assumptions` section requested by Round 1 is present and is
cross-checked in §1 below.

## 1. Verified-assumptions cross-check

| §2a claim | Verdict |
|---|---|
| Pre-routing seam has the authenticated user in scope (`completions.rs:57-80`) | CONFIRMED — `chat_completions_inner(State, user: AuthenticatedUser, HeaderMap, Json)` at `:57-61`; `maybe_downgrade` at `:79` |
| API keys are many-per-user; `AuthenticatedUser.api_key_id: Option<i64>`, `None` for legacy auth | CONFIRMED — `src/db/models.rs:12-14`, comment states the legacy-auth case |
| Unknown models cost $0 (`src/router/cost.rs`, `None => 0.0`) | CONFIRMED — `calculate_with_cache` match arm, `src/router/cost.rs:228` |
| Session affinity always defers to the newly resolved provider/model | CONFIRMED — `src/router/session_affinity.rs:94-117` |
| `/v1/responses` has no pre-routing seam | CONFIRMED — zero matches for `complexity_router\|maybe_downgrade` in `src/api/routes/responses.rs` |
| Budget rules can deny by model allow-list | CONFIRMED — `src/router/policy.rs:79` |
| `ProviderConfig` accepts new optional fields (no `deny_unknown_fields`) | CONFIRMED — `src/config/schema.rs:526-532` |
| `/admin/api/models/available` exists (commit 2971f2b, #34) | CONFIRMED — present in history |

No unverified assumption is doing load-bearing work. **The gap is what §2a does
not cover:** the response cache appears nowhere in the spec, and it sits
directly in the path the design modifies (§2.1, §2.2).

## 2. Literal-wrongness findings

**2.1 The classifier runs before the response-cache lookup, so cached requests pay for classification.**
Evidence: the pre-routing seam is `completions.rs:79`. The cache lookup is at
`completions.rs:263-275`, deliberately downstream — the comment at `:82-84`
states the key "must be built from the *resolved* model", and the key is
`completion_cache_key(&canonical_model, &body)` (`:266`). SmartRouter replaces
the seam at `:79`, so every smart-routed request issues a classifier LLM call
(2s budget, §6) before the system checks whether it already has the answer.
§6 requires classifier tokens to be written to the cost ledger as routing
overhead. A request that costs $0 today therefore acquires a non-zero cost
under smart routing.
Proposed fix: ladder membership is known from the policy without
classification. For cache-eligible requests, probe candidate cache keys for the
policy's ladder members before classifying, serve any hit, and classify only on
a miss. Classification then happens when the router is about to spend money,
not when it is about to save it.

**2.2 Smart routing fragments the response cache, and §9's savings figure does not account for it.**
Evidence: the cache key is per resolved canonical model (`:266`). Explore
traffic (`explore_rate`, default 0.10), capacity overflow, throttle cooldown,
and Phase-2 score drift all route identical request bodies to different ladder
members, producing a distinct key per member. Cache-hit rate is a first-class
operator metric (`/admin/cache`, README) and cache hits are reported as
savings. §9 defines "estimated dollars saved" as paid-tier price minus actual
cost, which counts smart routing's win but not the cache savings it displaced —
two cost-optimising subsystems, each reporting its own gain, one consuming the
other's.
Precedent: `VOLATILE_FIELDS` (`src/router/cache/mod.rs:43-51`) strips
`session_id` and attribution tags specifically so per-engagement tagging cannot
"fragment the cache into one entry per tag". Smart routing reintroduces
fragmentation on the one axis that cannot be stripped, since the model is
genuinely part of the answer's identity.
Proposed fix: state the interaction. The savings view must net cache savings
lost against routing savings gained, and the per-policy metric set gains a
cache-hit-rate-under-policy figure so the trade is visible rather than inferred.

**2.3 The estimated-pricing mechanism produces two prices for one request and under-costs new models.**
Evidence: §7 substitutes "the average per-token price of all priced members in
the same ladder" for an unpriced member, while the ledger keeps recording
actuals via the existing calculator, which returns `0.0` for the same model
(`src/router/cost.rs:228`). A model whose true price exceeds the ladder average
is under-costed in selection until an operator acts on an advisory alert —
nothing forces resolution, and the exploit rule prefers the cheapest member, so
the under-costed model attracts traffic. Any cost figure must then be qualified
by which of the two prices produced it.
Proposed fix (operator decision, 2026-09-02): make pricing a precondition for
smart routing rather than an estimate. A ladder member with neither a
`[[pricing]]` entry nor a `free = true` provider is rejected at policy save,
never auto-enrolled as a routable trial, and skipped in selection. This deletes
the estimate, the alert, the "estimated" labels, and the divergence between
routing price and ledger price. Scope is smart routing only — ordinary traffic
to unpriced models is unchanged, and the separate budget-enforcement hole
created by `None => 0.0` is logged as its own item, not resolved here.

## 3. Forced decisions

**3.1 `ComplexityRouter` and `SmartRouter` occupy the same seam with no stated precedence.**
The choice: §2 describes `ComplexityRouter::maybe_downgrade` as the seam "where
smart routing slots in", and §4 says SmartRouter "runs at the existing
pre-routing seam (where `ComplexityRouter` sits today)". Neither sentence says
whether complexity routing is replaced, or runs first, or is skipped when a
policy matches. An operator with `[routing.complexity]` enabled *and* a routing
policy assigned gets undefined behaviour, and both components silently rewrite
the caller's requested model — the failure class `strict_model_resolution`
exists to prevent.
Why it is forced: the two answer the same question (is this request hard enough
to need an expensive model?) at the same point in the handler and return the
same type (a model to dispatch to). No downstream component distinguishes them.
Options:
(a) **Merge (recommended).** SmartRouter subsumes complexity routing; the
    token-count heuristic becomes a classifier implementation
    (`classifier_kind = "token_threshold"`) alongside `"llm"`, with per-policy
    thresholds mapping an estimate to a tier. `[routing.complexity]` is
    translated at startup into an implicit two-tier policy, so existing
    deployments are unaffected and there is exactly one component that may
    rewrite the requested model. Also gives operators a zero-cost classifier
    for latency-sensitive policies.
(b) Keep both, state precedence explicitly (a matched policy suppresses
    complexity routing) and document `[routing.complexity]` as legacy.
(c) Delete complexity routing outright — smallest surface, but a breaking
    change for anyone using it.

## 4. Convention and completeness gaps

Items where the spec is not wrong but departs from patterns every comparable
feature in this repo follows. Each is a spec-text amendment.

**4.1 No Postgres migrations or dual repository implementations.** §5 says "new
migrations" and names no files. The Groups, Budgets and Keys specs each named
both `migrations/NNN_x.sql` and `migrations/postgres/NNN_x.sql` plus
`src/db/sqlite/` and `src/db/postgres/` implementations. CLAUDE.md requires
`cargo build --features postgres` to pass. Four new tables with no Postgres
counterpart is an omission, not a formality. Next free migration number is
**028** (highest present: `027_app_settings.sql`).

**4.2 No audit rows for policy mutations.** Every mutating admin action in every
prior spec writes a `NewAuditLogEntry` (`src/db/models.rs:412-419`) with a named
action string. Policy create/update/delete, assignment, experiment
create/close, and manual trial promotion write nothing. Assigning a routing
policy changes which model answers a user's requests — at least as
audit-worthy as disabling an API key.

**4.3 No admin dashboard page.** §5 specifies admin REST and CLI; §9 says
`/admin/stats` "gains a per-policy view". Every operator-facing feature since
April ships a dedicated HTMX page with a nav link — `templates/admin/base.html`
lists fourteen (`:50-63`), including Groups, Budgets, Reports, Models, Cache and
Webhooks. Routing policies are DB-managed CRUD over a ladder of members,
structurally the same shape as Groups (cards, member tables, per-member
add/disable), and the trial and experiment comparisons are report surfaces that
should reuse the Reports panel + D3 conventions. As written, an operator can
create a group in the browser but must use the CLI to build a routing ladder.

**4.4 The `*Row` intermediate-struct convention is not mentioned** for the new
tables. Phase 12's plan records this as a standing pitfall: sqlx
deserialization goes through a private `*Row` struct with a `From` impl in
`src/db/sqlite/`, and every added column must be updated in both.

**4.5 `virtual_model` names are not advertised by `/v1/models`.** The rationale
for `RequestRouter::alias_map()` (`src/router/engine.rs:41-44`, issue #25) is
that `/v1/models` must advertise the names callers can actually route with,
because on config-alias-only deployments they exist nowhere else. A policy's
`virtual_model` (e.g. `"auto"`) is exactly such a name.

**4.6 Policy hot-reload behaviour is unspecified.** The codebase has both
patterns: DB aliases hot-swap through `ArcSwap` and take effect on the next
request (`src/router/engine.rs:36`, `update_db_aliases`), while webhooks take
effect on restart (README). Policies are operator-edited and read on every
request, so they want the alias pattern — but the spec does not say, and the
difference is an `ArcSwap`-vs-`RwLock`-vs-restart decision an implementer will
otherwise make arbitrarily.

**4.7 `routing_policies.tiers` as a JSON blob blocks the reverse lookup the
pricing gate needs.** With the ladder stored as JSON, "which policies reference
model X" is unanswerable in SQL — required when a `[[pricing]]` entry is
removed, when a provider is disabled, and at policy-save validation. Groups
went relational for the analogous membership structure. A
`routing_policy_members` table (policy_id, tier_index, position, provider,
model, trial, trial_match) preserves ordering, matches precedent, and makes the
gate enforceable.

**4.8 Five registries answer one question with no shared entry point.** §4 step
3 consults the circuit breaker, the operator availability map, throttle state,
capacity, and (per R1 2.1) the user's allow/deny rules to decide whether a
member can serve this request. Two of those are new. A single façade — one call
returning usable/unusable plus the reason — gives the `overflow_reason` metric
one place to be computed and keeps the skip rule from drifting between
selection and escalation paths.

**4.9 `ProviderCapacity` duplicates an existing shape.** `ConcurrencyLimiter`
(`src/router/concurrency.rs`) is `DashMap<i64, Arc<Semaphore>>` keyed by user,
and its header documents a known limitation: the cap is fixed at first use and
only a restart applies a changed limit. A second per-provider capacity
component should either reuse that shape deliberately or state why it differs —
and should not silently inherit the same reconfiguration limitation.

## 5. Recommendation

🛑 **Surface forced decision to user** — §3.1 needs a pick before implementation
(recommended: (a) merge).

§2.1 and §2.2 require a hot-path ordering change (cache probe before
classification) plus an honest savings metric; §2.3 is settled by the pricing
gate. All §4 items are spec-text amendments — none requires re-architecting,
though 4.3 and 4.7 change the delivered surface and should be priced into
Phase 1 rather than deferred.
