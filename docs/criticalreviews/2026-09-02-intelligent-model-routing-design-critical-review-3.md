# Critical Design Review: 2026-09-02-intelligent-model-routing-design (Round 3)

**Spec:** `docs/superpowers/specs/2026-09-02-intelligent-model-routing-design.md` (revision 8)
**Verified Assumptions section:** PRESENT (§2a)
**Prior rounds:** review-1 (internal coherence), review-2 (architectural fit)

Round 3 reviews revisions 4–8 — the material added after round 2 — against the
stated requirement that **modelrouter must be completely robust**. The lens is
therefore: what can fail, and does the spec keep its own promise that a routing
failure never makes a request less servable than smart routing being off?

## 0. Coverage enumeration — new material since round 2

| Row | Disposition |
|---|---|
| §4 pipeline (plugin invocation, validation gate) | ok — order is explicit, fallback defined at each step |
| §4a `ProviderCapacity` | in-flight counter has no stated release discipline → §2.4 |
| §4b `MemberHealth` | ok — single façade, reason enum matches `overflow_reason` |
| §4c `Router` plugin API | containment claim overstated → §2.2; isolation question forced → §3.1 |
| §4d request objective | queue has a time bound but no depth bound → §4.1; interaction with re-classification unstated → §4.4 |
| §4e enforced validity | ok — strongest section in the document; boundary table is complete against the surfaces named elsewhere |
| §5 storage (overlay, three tiers) | DB-unavailable-at-startup behaviour unstated → §4.3 |
| §6 classification | no breaker on a wedged classifier → §2.3 |
| §7.0 pricing gate | ok — transactional at write, single store |
| §7.0a shadow traffic | no shedding under pressure → §4.2 |
| §8 error table | rows are individually right; the table has no row for the cache probe itself → §2.1 |
| §9 observability | ok — every clamp and rejection counted |
| §13 decisions + open | ok — alternatives preserved with the evidence that would reopen them |

**Rules and operands**

| Row | Disposition |
|---|---|
| "Probe cost is an in-memory lookup per member, bounded and off the network" | **false on the Redis backend** → §2.1 |
| "Panics are contained (`catch_unwind`)" | true only under `panic = "unwind"`, which nothing pins → §2.2 |
| "A policy can never make a request less servable than today" | asserted in five places, never stated once as a testable invariant → §4.5 |
| Validation gate applies to built-ins identically | ok — stated in §4e and exercised in §10 |
| Fail-closed on caller directives / degrade on internal signals | ok — consistently applied across the §4e table |

## 1. Verified-assumptions cross-check (new claims only)

| Claim | Verdict |
|---|---|
| `app_settings` overlay exists with file-fallback semantics | CONFIRMED — `migrations/027_app_settings.sql`, comment states the rule verbatim |
| `AppState.live_settings: Arc<ArcSwap<Settings>>` | CONFIRMED — `src/api/app.rs:74` |
| Cargo features already gate optional components | CONFIRMED — `otel`, `postgres`, `bedrock`, `prometheus`; release workflow builds a Docker variant per set |
| `catch_unwind` is available (unwind, not abort) | CONFIRMED **but unpinned** — `Cargo.toml` has no `[profile.release]`, so this is the default, not a guarantee → §2.2 |
| Response cache is in-memory | **CONTRADICTED** — `src/router/cache/mod.rs:21` documents two backends, `memory` and `redis` → §2.1 |
| Cache exposes backing-store reachability | CONFIRMED — `src/router/cache/mod.rs:202`, "for Redis this is a real PING" |

## 2. Literal-wrongness findings

**2.1 The cache probe is specified as free and off-network. On the Redis backend it is neither, and it now sits in front of every smart-routed request.**
Evidence: §4 step 2 states "probe cost is an in-memory lookup per member (5–15),
bounded and off the network." `src/router/cache/mod.rs:21` documents the store
as `memory` (per-process) **or** `redis` (shared across stateless replicas).
Revision 6 moved the probe ahead of classification and revision 4 widened it to
all ladder members, so on a Redis deployment the design as written performs
5–15 network round trips before the router has decided anything — on the hot
path of every request, replacing what used to be one lookup after resolution.
A slow or unreachable Redis converts the router's cheapest step into its most
expensive one. There is also no `§8` row for a cache probe that hangs; the only
probe row covers an *error*.
Proposed fix: (a) issue the probe as a single multi-key operation (`MGET`) on
Redis rather than N sequential gets, (b) put the probe under a hard budget of a
few milliseconds and treat exceeding it as a miss, (c) skip the probe entirely
when the store reports itself unreachable — the reachability signal already
exists at `cache/mod.rs:202` — and (d) add the timeout row to §8. The probe is
an optimisation; it must never be able to slow the path it exists to shorten.

**2.2 Panic containment is asserted more strongly than the build guarantees.**
Evidence: §4c states "plugin invocation is wrapped in `catch_unwind`; a
panicking plugin abstains." `Cargo.toml` declares no `[profile.release]`, so
the crate inherits `panic = "unwind"` and this holds *today* — by default, not
by decision. Anyone adding `panic = "abort"` (a routine size/perf change) makes
every plugin panic an immediate process abort, silently deleting the guarantee
with no compile error and no test failure. Two further gaps: `catch_unwind`
across an `await` requires care that the spec does not describe, and a plugin
that panics while holding a lock poisons it for every subsequent request, so
"the plugin abstains" understates the damage.
Proposed fix: pin `panic = "unwind"` in `[profile.release]` with a comment
naming this guarantee as the reason, and add a test that a panicking plugin
abstains — so the profile change breaks a test rather than a production
invariant. State plainly that lock poisoning is not covered, and that
compiled-in plugins are trusted code (§3.1).

**2.3 A wedged classifier degrades every request at full cost, because nothing breaks the circuit.**
Evidence: §6 gives the LLM classifier a 2s budget and §8 degrades a timeout to
`default_tier` (or the heuristic). Both are *per request*. Nothing makes the
degradation sticky, so a classifier that is hung rather than erroring — a
wedged local ollama, the exact recommended deployment — costs a full 2s on
every smart-routed request for as long as it stays wedged, driving p99 latency
across the whole deployment to 2s+ while still, eventually, routing correctly.
`CircuitBreaker` exists for exactly this shape of failure
(`src/router/circuit_breaker.rs`) and is applied to providers but not to the
classifier, which is the one upstream call the router itself makes.
Proposed fix: put the classifier behind the existing `CircuitBreaker`. After N
consecutive timeouts the breaker opens and classification is skipped entirely —
straight to the heuristic or `default_tier` at zero latency — with periodic
half-open probes to recover. This also removes the §6 correlated-failure
scenario's teeth: a busy local instance stops being a per-request tax.

**2.4 `ProviderCapacity` has no stated release discipline, so a leaked counter sidelines a member permanently.**
Evidence: §4a describes a "per provider/model in-flight counter with a
configured cap" and never says how it is decremented. Every early return in the
dispatch path — validation rejection, escalation, panic, client disconnect,
streaming abort — must release it. A counter that leaks on any of these paths
drifts monotonically toward the cap, at which point `MemberHealth` reports the
member at capacity forever and the ladder silently loses its cheapest rung.
Nothing in the design detects this: overflow-to-next-tier is normal behaviour,
so a permanently sidelined free model looks like healthy overflow while quietly
costing money.
Proposed fix: acquire an RAII guard whose `Drop` releases, the way
`ConcurrencyLimiter` already yields an `OwnedSemaphorePermit`
(`src/router/concurrency.rs`), rather than incrementing and decrementing by
hand. Add a saturation metric and a startup-time assertion that idle in-flight
counts are zero.

## 3. Forced decisions

**3.1 "Completely robust" and "compiled in" are in tension for third-party plugins. How far does the requirement reach?**
The choice: revision 8 makes plugins compiled-in for efficiency and type safety,
and §4c is honest that containment is "damage limitation, not a sandbox" — a
compiled-in plugin runs with the router's privileges, can block the executor and
starve every other request, can poison locks, and (under `panic = "abort"`) can
end the process. If "completely robust" means *no plugin can degrade the
router*, in-process plugins cannot satisfy it and the escape hatch is the
primary mechanism after all.
Why it is forced: the two properties are traded against each other by the
mechanism itself, not by tuning. Options:
(a) **Trusted-code stance.** Compiled-in plugins are first-party or reviewed
    code, robustness comes from tests and review, and the spec says so plainly.
    Third parties who need isolation use `http`. Cheapest, honest, and matches
    how `hook_permissions` already treats hooks as privileged.
(b) **Isolation for untrusted plugins.** Anything not first-party runs
    out-of-process via `http`, and the docs say in-process is for code you own.
    Same as (a) but with the boundary written into policy rather than left to
    judgement.
(c) **WASM.** True sandboxing, real fuel/timeout limits, language-agnostic, no
    ABI problem — and a substantial subsystem to build and maintain.
Recommendation: (a) or (b). (c) is a project, not a section.

## 4. Robustness gaps

Additive; none contradicts the design, each is a way the system can degrade
badly under load or partial failure.

**4.1 The cost-path queue is bounded in time but not in depth.** §4d gives
`max_queue_ms` and no maximum queue length, so a saturated cheap tier under
sustained load accumulates waiters until memory does the shedding. Bound the
depth and shed to the next tier when full — an immediate overflow is the
behaviour the queue was an exception to, so the fallback is already defined.

**4.2 Shadow traffic has no pressure valve.** §7.0a mirrors a fraction of
requests unconditionally. The moment a deployment is struggling is the moment it
should stop paying double for evidence. Shed shadow when the primary's breaker
is open, when `ProviderCapacity` is near cap, or when latency exceeds a
threshold — and record the shed so the sample loss is visible rather than
inferred.

**4.3 Startup with an unavailable database is unspecified.** Revision 5 moved
policies into the DB. The overlay model implies the right answer — no rows means
"use the file" — so a DB outage at startup should degrade to file-seeded
defaults or to no-policy routing, and serve. But the spec never says it, and the
alternative implementation (fail to start) is equally consistent with the text.
State it: **the router must start and serve with the database unavailable**,
routing as if no policy existed. This is the strongest argument for having kept
the file tier at all and should be written down as such.

**4.4 Re-classification on quality failure ignores the request's objective.**
13.8 re-classifies on refusal/truncation, adding a second classifier call to an
already-failed request. On a `latency` request that is the worst possible moment
to spend 2s: the caller is waiting and has already lost one round trip. Skip
re-classification on the latency path, or bound it to the heuristic there.

**4.5 The central invariant is asserted but never stated.** "A policy can never
make a request less servable than today" appears in §4 step 4, §4c, §8 and twice
in §13, each time as prose about a specific path. It is the spec's most
important claim and the one a reviewer most wants mechanised. State it once, as
a named invariant with a test obligation: *for every failure mode in §8, a
request that would succeed with `smart_routing.enabled = false` still succeeds.*
Then §10 can assert it per row rather than sampling it.

## 5. Recommendation

🛑 **Surface forced decision §3.1** — the trusted-code boundary for compiled-in
plugins needs stating before implementation, since it determines what the
robustness requirement actually promises.

§2.1 and §2.3 are the two findings that matter for a production deployment:
both convert an intermittent upstream problem into a whole-deployment latency
regression, and both are fixed with machinery that already exists in the repo
(multi-key cache reads plus the reachability signal; `CircuitBreaker`). §2.2 and
§2.4 are small changes that prevent silent loss of a stated guarantee. §4 items
are additive hardening, and §4.5 is the one that would let the rest be tested
rather than reviewed.
