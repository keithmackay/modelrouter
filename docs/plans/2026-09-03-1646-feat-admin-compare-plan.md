---
title: Admin compare page for model, provider, tag and run arms - Plan
type: feat
date: 2026-09-03
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
execution: code
product_contract_source: ce-plan-bootstrap
origin: docs/superpowers/specs/2026-09-02-intelligent-model-routing-design.md
---

# Admin compare page for model, provider, tag and run arms - Plan

## Goal Capsule

- **Objective:** ship `/admin/compare` — a dashboard page and JSON endpoint that put two arms side by side (two models, two providers, two tag values, or two runs) over the data the router already records, with the honesty rules the spec demands, and document end to end how a client application runs such an experiment and reads its results.
- **Authority:** the spec at `docs/superpowers/specs/2026-09-02-intelligent-model-routing-design.md` (revision 16), §7b "Comparison analysis" and §13.20 "compare page ships first". Settled decisions there are not reopened here.
- **Execution profile:** one feature branch, one PR, ordinary repo verification (`cargo test`, `cargo build --features postgres`, `cargo clippy`). No schema migration.
- **Stop conditions:** the Variant and Pair dimensions, the experiments table, mirroring, the feedback endpoint and any quality signal are out of scope; if a unit appears to need any of them, stop and leave the item in Scope Boundaries rather than pulling it in.
- **Tail ownership:** implementation and review continue in this session; the privacy grep in CLAUDE.md runs before every commit.

---

## Product Contract

### Summary

Operators can already ask "what did this tag cost" on `/admin/reports` and `/admin/api/usage/attribution`. They cannot ask "is arm A cheaper, faster or more reliable than arm B", which is the question every model experiment ends in. This plan adds a comparison surface over the existing `cost_ledger`, `prompts` and `request_failures` tables: pick a dimension, pick two values, get per-arm metrics with deltas and three charts, on the dashboard and as JSON. A client application forms the arms itself — by choosing a model per arm and tagging each request — so the guide written here is what makes the feature usable from the outside.

### Problem Frame

Downstream callers want to run A/B experiments across models or prompt variants without building their own accounting. The router already persists everything needed per request: routed model, provider, tokens, cost, cache hit, latency and the caller's attribution tags and correlation id. What is missing is a query that partitions those rows into two arms and a page that shows the two partitions honestly — cost and latency have different denominators, some models are unpriced, and nothing here measures quality. §13.20 sequences this page first precisely because it is useful with no new write paths.

### Requirements

**Query and endpoint**

- R1. `CostRepository` accepts an arm filter with four variants — model, provider, tag key/value, correlation id — and returns totals (requests, cost, saved, tokens in/out, cache hits) and a per-day breakdown for that arm within a time window, on both SQLite and Postgres.
- R2. `GET /admin/api/compare?dimension=<model|provider|tag|run>&a=<value>&b=<value>&window=<all|daily|weekly|monthly>[&key=<tag key>]` returns a JSON document with both arms' metrics, the deltas, and the honesty annotations; it requires an admin JWT like `/admin/api/usage/attribution`. The tag dimension partitions on exactly one key: rows are matched on that key's value alone, so two experiments that share an arm value (`arm=a` in both) are not separated by this query — see R18 for the naming convention that keeps them apart.
- R3. A tag key arriving in a query is re-validated with `api::attribution::is_safe_tag_key` before it reaches any SQL, and the JSON path is bound as a parameter, never interpolated into the statement.
- R4. Malformed input (unknown dimension, missing value, `a == b`, unsafe key, unknown window) returns 400 with a message naming the field; nothing panics and no query runs.
- R5. Every comparison query is bounded: window-limited, aggregate-only, and the percentile lookup is `ORDER BY … LIMIT 1 OFFSET n` (nearest rank, KTD4) so no latency sample is buffered application-side regardless of row count.

**Metrics**

- R6. Each arm reports: request count; total cost and cost per request; tokens in and out with per-request figures; mean, p50 and p95 latency with the latency sample count; cache hits and hit rate; failure count and error rate.
- R7. The delta column shows B minus A and percentage change for every numeric metric; per-request figures are the default reading and totals are visible but secondary.
- R8. Latency comes from `prompts.latency_ms`, whose row count differs from the ledger's; the page states the sample count next to every latency figure and shows a dash rather than a number when the sample is zero.
- R9. Time to first token is not recorded by the router today; the page says so in place rather than showing a zero.
- R10. An arm whose rows include a model with no pricing entry is badged "unpriced" and its cost figures are marked as incomplete.
- R11. The page states, in place, that it carries no quality column and that a difference in cost or latency is not evidence of a difference in quality.
- R12. Error rate is failures divided by (ledger requests + failures), with failures counted from `request_failures` for the same arm and window.

**Dashboard**

- R13. `/admin/compare` renders under `DashboardSession` with a filter header (dimension, tag key when the dimension is tag, arm A, arm B, window) and an HTMX-swapped panel body served by `/admin/compare/panels`, following `reports.html` / `reports_panels.html`.
- R14. A Compare link appears in the nav after Reports.
- R15. Value pickers offer the values the ledger actually contains for the chosen dimension (models, providers, tag values for the key, correlation ids), capped at the same facet limit the attribution facets use.
- R16. Three charts render from the JSON the panels already carry, using the vendored `/static/d3.js`: a grouped bar of per-request cost, tokens and latency for A vs B; a two-line daily cost chart; and a p50/p95 latency comparison.

**CLI and documentation**

- R17. `modelrouter report compare` runs the same query from the terminal with `--dimension`, `--a`, `--b`, `--window`, `--key` and `--format table|json`.
- R18. A guide under `docs/` shows a client application exactly how to run an experiment today — assign each request to an arm with attribution, choose the model per arm client-side — and how to retrieve the results by JWT and by CLI and view them on the dashboard, with working request and response examples. Because the compare query partitions on one tag key (R2), the guide's convention puts the experiment name in the arm value — `"arm": "checkout-v2:a"` / `"arm": "checkout-v2:b"` — so concurrent experiments never share a value; the `experiment` tag stays for the attribution report.
- R19. README, CLAUDE.md's endpoint table and CHANGELOG record the new surface.

### Key Decisions

- KD1. **Arms are formed client-side.** The router does not assign arms, mirror requests or store an experiment definition (spec §13.20). The client picks the model for each arm and tags every request with the experiment and arm; the comparison page partitions on what was recorded. Governs R18.
- KD2. **Four dimensions only.** Model, provider, tag and run are the dimensions the ledger supports today; Variant and Pair wait for §7a/§7.0a. Governs R1, R2.
- KD3. **Honesty over completeness.** A metric that cannot be computed from recorded data is shown as absent with a reason, never as zero. Streaming responses are the one place recorded data is itself approximate today: the completions stream estimates tokens from character count, and the messages stream records zero completion tokens and a fixed placeholder latency. The comparison cannot tell those rows apart from measured ones, so the JSON `caveats` array and the page say so, and the guide steers experiment traffic to `stream: false`. Governs R8, R9, R10, R11.

### Scope Boundaries

- Deferred to Follow-Up Work
  - Variant and Pair dimensions, the experiments table, prompt mirroring, the feedback endpoint (§7a, §7.0a, §13.21+).
  - Recording time to first token on streaming responses; once it exists it slots into the latency block.
  - Accurate token usage and measured latency for streaming responses (today: estimated tokens on the completions stream, zero tokens and a placeholder latency on the messages stream); until then the comparison carries a streaming caveat (KD3).
  - Partitioning on more than one tag key at once (experiment *and* arm); §7a's Variant dimension removes the single-key limit by giving arms an identity of their own. Until then the arm-value naming convention in R18 keeps concurrent experiments apart.
  - An index on `prompts(created_at)`; the comparison query scans the same way the existing attribution reports do, and the index is a separate, measured change.
  - Wiring the incomplete-pairs count (§7.0a) into the coverage line; the coverage line is designed to receive it.
- Outside this feature
  - Any quality metric, judge or preference signal.
  - Statistical significance testing; the page shows deltas, not p-values.

---

## Planning Contract

### Key Technical Decisions

- KTD1. **`ArmFilter` wraps `AttributionFilter`.** The new enum has `Model`, `Provider` and `Attribution(AttributionFilter)` variants so tag and run reuse the predicate code and JSON-path handling that already exist in both backends. Chosen over a separate four-variant enum because the two attribution variants would otherwise duplicate the predicate twice per backend.
- KTD2. **Bind the JSON path on SQLite.** `distinct_attribution_values` already binds `json_extract(attribution_tags, ?)`; the arm predicate follows it, and `attribution_predicate` is switched to the same form in passing so the interpolating variant disappears. Postgres already binds (`attribution_tags::jsonb ->> $1`). The handler still re-validates the key so an unsafe key is rejected with a 400 before the repository sees it.
- KTD3. **Latency and failures come from their own repositories, not a join.** `prompts` may live in a separate database (`[storage] prompt_db_path`), so the comparison runs one aggregate query per source — ledger on `state.db`, latency on `state.prompt_db`, failures on `state.db` — and assembles the result in Rust. Chosen over a join, which would be impossible in the split-database configuration.
- KTD4. **Percentiles by offset, nearest rank.** p50 and p95 are `SELECT latency_ms … ORDER BY latency_ms LIMIT 1 OFFSET (ceil(n*q) - 1)` with `n` from a preceding count and the offset clamped to `[0, n-1]` (so `n = 1` yields offset 0). Nearest rank keeps the tail visible: for samples 100, 200, 300, 400, 1000 it reports p95 = 1000, where `floor((n-1)*q)` would report 400. Portable across SQLite and Postgres, no application-side buffering of the sample, no window-function dependency. Chosen over loading the sample into memory.
- KTD5. **Latency sample excludes cache hits.** `record_cache_hit` writes `latency_ms = 0` as a sentinel, so the sample is `latency_ms IS NOT NULL AND latency_ms > 0`. Cache hits still count in requests and hit rate; the coverage line shows the resulting denominator difference.
- KTD6. **Model arm predicate.** Ledger rows carry the routed model in `model`, so `Model(x)` matches `cost_ledger.model = x`, `prompts.routed_model = x` and `COALESCE(request_failures.routed_model, request_failures.request_model) = x`.
- KTD7. **One builder, three consumers, no `AppState` dependency.** `build_comparison(sources: &CompareSources, query: &CompareQuery) -> Result<Comparison>` lives beside `build_report` in the admin module and serves the JSON endpoint, the dashboard panels and the CLI. `CompareSources { db: Arc<dyn DatabaseProvider>, prompt_db: Arc<dyn DatabaseProvider>, cost_calc: Arc<CostCalculator> }` is the builder's only input besides the query: the admin handlers build it from `AppState` (`state.db`, `state.prompt_db`, `state.cost_calculator`), and the CLI builds it from settings — the main database, `settings.storage.prompt_db_path` when set or the main database otherwise, and `CostCalculator::new_with_config(&settings.pricing)`. This is what lets the CLI reach the builder: `Commands::Report` opens a bare database and calls repositories directly (`report_attribution` is that pattern, not a `build_report` consumer), and never constructs `AppState`.
- KTD8. **Unpriced detection through the calculator.** `CostCalculator` gains `has_price(model) -> bool`; the builder asks it for each distinct model in the arm (from the per-model breakdown) and badges the arm if any is unpriced.

### High-Level Technical Design

Data flow for one comparison, whichever consumer asks:

```mermaid
flowchart TB
  Q[CompareQuery: dimension, key, a, b, window] --> V[validate: dimension known, a != b, key safe, window known]
  V --> F[two ArmFilters + window range]
  F --> L[CostRepository on state.db: totals, by_day, by_model per arm]
  F --> P[PromptRepository on state.prompt_db: latency count, mean, p50, p95 per arm]
  F --> E[FailureRepository on state.db: failure count per arm]
  L --> B[build_comparison: per-arm metrics, deltas, unpriced badge, coverage line]
  P --> B
  E --> B
  B --> J[GET /admin/api/compare JSON]
  B --> H[/admin/compare/panels HTML + chart JSON]
  B --> C[report compare CLI table or JSON]
```

The page is two templates: `compare.html` carries the filter header and an empty `#panels` target that loads on page load and on every selector change; `compare_panels.html` carries the metric table, the coverage line, the caveats and the three charts. Changing the dimension re-renders the whole page (the value pickers depend on it); changing a value or window swaps only the panels.

### Assumptions

- A1. Latency is unavailable when `[storage] store_prompts = false` or when a request skipped logging; the page reports the sample count as zero and shows dashes, which is the R8 behaviour rather than a new case.
- A2. `request_failures` rows carry attribution and routed model in the same columns as the ledger, so the four arm predicates apply to all three tables with only the model column differing (KTD6).
- A3. The guide lives at `docs/experiments.md` with a README pointer; the existing README attribution section is the natural anchor.
- A4. The run picker lists at most 500 correlation ids, the same cap the attribution facets accept. The existing `distinct_attribution_values(None, FACET_LIMIT)` orders ids ascending and would return the *oldest* 500, so the picker uses the new `distinct_recent_correlation_ids(limit)` from U1, ordered by each id's latest ledger activity, and a deployment with more than 500 runs sees the most recent 500.

---

## Implementation Units

### U1. Arm filter and aggregate queries in CostRepository

- **Goal:** the ledger can be partitioned by any of the four arm kinds and summarised per arm and per day.
- **Requirements:** R1, R3, R5; KTD1, KTD2, KTD6.
- **Dependencies:** none.
- **Files:** `src/db/repositories/costs.rs`, `src/db/sqlite/costs.rs`, `src/db/postgres/costs.rs`.
- **Approach:** add `ArmFilter { Model(String), Provider(String), Attribution(AttributionFilter) }` with a `label()` for display. Add trait methods `arm_totals`, `arm_by_day` and `arm_by_model`, each taking `(&ArmFilter, start, end)` and returning the existing `AttributionTotals` / `AttributionBreakdownRow` shapes. In each backend add an `arm_predicate` that returns a SQL fragment plus bind values; on SQLite bind the JSON path as a parameter and rework `attribution_predicate` to do the same. Reuse `TOTALS_SELECT`, `totals_from_row` and `breakdown_from_row`. Add two picker helpers to the trait and both backends: `distinct_providers_in_ledger() -> Vec<String>` (sorted, mirroring `distinct_models_in_ledger`) for the provider picker, and `distinct_recent_correlation_ids(limit) -> Vec<String>` (`GROUP BY attribution_correlation_id ORDER BY MAX(created_at) DESC LIMIT ?`, null ids excluded) for the run picker (A4).
- **Patterns:** `attribution_totals` / `attribution_by_day` in both backends; `distinct_attribution_values` for the bound JSON path.
- **Test scenarios:**
  - Happy path: three ledger rows for model X and two for model Y in the window; `arm_totals(Model(X))` returns requests 3 with the summed cost and tokens, `Model(Y)` returns 2.
  - Provider arm counts only rows with that provider; a row for the same model on another provider is excluded.
  - Tag arm with key `arm` value `a` matches rows whose `attribution_tags` JSON carries `"arm":"a"` and ignores rows where the key is absent or the value differs.
  - Run arm matches the correlation id exactly, not as a prefix.
  - Window edge: a row one second before `start` is excluded; a row at `start` is included.
  - `arm_by_day` returns one row per day in ascending order with the day's totals.
  - `distinct_providers_in_ledger` returns each provider once, sorted; `distinct_recent_correlation_ids(2)` on three runs returns the two with the latest ledger rows, newest first, and omits rows with no correlation id.
  - Existing attribution tests in `src/db/sqlite/costs.rs` and `tests/test_attribution.rs` still pass after the predicate change.
  - Postgres runtime (`tests/test_compare_postgres.rs`, `#[cfg(feature = "postgres")]`, `#[ignore]`, connects to `MODELROUTER_TEST_POSTGRES_URL`): the arm-totals, tag JSON-path, percentile-offset and failure-count queries return the same figures as the SQLite scenarios above. Runs only where a Postgres is provided; it is not part of the default `cargo test` and no Postgres is available in the development environment, so it documents the expectation rather than proving it here.
- **Verification:** `cargo test costs`, `cargo build --features postgres`; `cargo test --features postgres --test test_compare_postgres -- --ignored` where `MODELROUTER_TEST_POSTGRES_URL` is set.

### U2. Latency summary and failure count per arm

- **Goal:** per-arm latency statistics from `prompts` and failure counts from `request_failures`, both bounded.
- **Requirements:** R5, R8, R12; KTD3, KTD4, KTD5, KTD6.
- **Dependencies:** U1 (uses `ArmFilter`).
- **Files:** `src/db/repositories/prompts.rs`, `src/db/sqlite/prompts.rs`, `src/db/postgres/prompts.rs`, `src/db/repositories/failures.rs`, `src/db/sqlite/failures.rs`, `src/db/postgres/failures.rs`.
- **Approach:** add `LatencySummary { samples, mean_ms, p50_ms, p95_ms }` and `PromptRepository::latency_summary(&ArmFilter, start, end)`. One query returns count and average over the sample predicate; when the count is positive, two more return the offset rows for p50 and p95. Add `FailureRepository::count_for_arm(&ArmFilter, start, end)`. Both backends share the predicate mapping with U1 but on their own column names (`routed_model`, `COALESCE(routed_model, request_model)`).
- **Patterns:** `count_by_stage` in the failures backends; `purge_older_than` for the prompts window predicate.
- **Test scenarios:**
  - Happy path: latencies 100, 200, 300, 400, 1000 for one model → samples 5, mean 400, p50 300, p95 1000.
  - A cache-hit row with `latency_ms = 0` and an audio row with `latency_ms = NULL` are excluded from the sample but present in the table.
  - Zero matching rows → `samples 0`, and mean/p50/p95 are `None`; no offset query runs.
  - Single sample → p50 and p95 both equal that value.
  - Failure count: two failures with `routed_model = X`, one with `routed_model = NULL, request_model = X`, one for Y → `Model(X)` counts 3.
  - Tag and run arms on failures match on the attribution columns.
  - Provider arm: prompts and failures rows for the same model on two providers → `latency_summary(Provider(p))` and `count_for_arm(Provider(p))` count only that provider's rows.
- **Verification:** `cargo test prompts failures`, `cargo build --features postgres`; the Postgres runtime test in U1 covers the percentile and failure-count SQL on that backend.

### U3. Price presence on the cost calculator

- **Goal:** the builder can tell whether a model's cost was actually computed.
- **Requirements:** R10; KTD8.
- **Dependencies:** none.
- **Files:** `src/router/cost.rs`.
- **Approach:** add `has_price(&self, model: &str) -> bool` using the same prefix-strip and lowercase normalisation `calculate_with_cache` applies, so the answer matches what the ledger recorded.
- **Patterns:** `calculate_with_cache`'s model normalisation.
- **Test scenarios:**
  - A configured model returns true with and without a provider prefix and regardless of case.
  - An unknown model returns false.
  - A model added through `new_with_config` returns true.
- **Verification:** `cargo test cost`.

### U4. Comparison builder and JSON endpoint

- **Goal:** one validated query produces the full comparison document for every consumer.
- **Requirements:** R2, R3, R4, R6, R7, R8, R9, R10, R11, R12; KTD3, KTD7, KTD8.
- **Dependencies:** U1, U2, U3.
- **Files:** `src/api/admin/compare.rs` (new), `src/api/admin/mod.rs`, `src/api/app.rs`.
- **Approach:** `CompareQuery { dimension, key, a, b, window }` with a `validate()` that maps to `(ArmFilter, ArmFilter, start, end)` or an `ApiError::InvalidRequest` naming the field; the tag dimension requires `key` and re-validates it with `is_safe_tag_key`. `build_comparison(&CompareSources, &CompareQuery)` (KTD7) runs the six repository calls, computes per-request figures, deltas (absolute and percent, `None` when A is zero), hit rate, error rate, the unpriced badge and the coverage line (`latency_samples` vs `requests` per arm), and sets `ttft: null` with a `ttft_note`. `Comparison` derives `Serialize` and carries a `caveats` array with two fixed entries: the quality statement, and the streaming statement (streamed responses record estimated or zero tokens and, on the messages API, a placeholder latency; compare non-streamed traffic). Add `CompareSources::from_state(&AppState)` for the handlers. Route `GET /admin/api/compare` under `AdminSession`. Windows reuse `attribution::window_range`.
- **Patterns:** `AttributionQuery::filter`, `build_report`, `get_attribution_usage` in `src/api/admin/attribution.rs`.
- **Test scenarios:**
  - Happy path (integration, `tests/test_compare.rs`): drive requests through the mock provider tagged `arm=a` on one model and `arm=b` on another, wait for ledger rows, call the endpoint with `dimension=tag&key=arm&a=a&b=b`; both arms report the expected request counts, per-request cost and tokens; deltas have the right sign. The in-process mock provider answers instantly with fixed tokens and never fails, so latency and failures are not expected from this path.
  - Latency and failures (same file): seed `prompts` rows with known `latency_ms` values and `request_failures` rows directly through `PromptRepository::create` / `FailureRepository::create` on the test state's databases, the way `tests/test_dashboard.rs` seeds failures; then the endpoint reports the expected sample count, mean, p50, p95, failure count and error rate per arm.
  - `dimension=model` on the two routed models gives the same partition as the tag query.
  - `dimension=provider` with ledger, prompts and failures rows seeded for two providers: the full JSON document for both arms matches an expected fixture (requests, cost, tokens, latency block, failure count, error rate, deltas).
  - `dimension=run` with two correlation ids.
  - `caveats` carries the quality and streaming statements on every response.
  - Error paths: unknown dimension → 400 naming `dimension`; `a == b` → 400; tag dimension without `key` → 400; `key=bad key!` → 400 with the existing safe-key message and no query executed; unknown window → 400; no JWT → 401; viewer-role JWT is accepted if `AdminSession` accepts viewers today, otherwise 403 (match the attribution endpoint).
  - Edge: an arm with zero rows returns zeros, `None` percentiles, dashes in the coverage line and no division by zero in deltas.
  - Unpriced: an arm whose model is absent from pricing carries `unpriced: true`; the other arm carries `false`.
  - Error rate: a seeded `request_failures` row for arm A raises A's failure count and error rate and leaves B's at zero.
- **Verification:** `cargo test --test test_compare`, `cargo test --test test_attribution`.

### U5. Dashboard page, panels, nav and template registration

- **Goal:** operators reach the comparison from the dashboard with pickers populated from real data.
- **Requirements:** R13, R14, R15, R8, R9, R10, R11.
- **Dependencies:** U4.
- **Files:** `templates/admin/compare.html` (new), `templates/admin/compare_panels.html` (new), `templates/admin/base.html`, `src/api/admin/templates.rs`, `src/api/admin/compare.rs`, `src/api/app.rs`.
- **Approach:** `get_compare` (`DashboardSession`) builds picker options for the chosen dimension — `distinct_models_in_ledger`, `distinct_providers_in_ledger` from U1, `distinct_attribution_tag_keys` plus `distinct_attribution_values(key)`, or `distinct_recent_correlation_ids(FACET_LIMIT)` from U1 — and renders `compare.html` with the current selection. The dimension and key selectors navigate (`hx-get` to `/admin/compare` with `hx-target="body"` or a plain form GET); the value and window selectors `hx-get` `/admin/compare/panels` into `#panels`. `get_compare_panels` reuses `build_comparison` and renders the metric table (per-request first, totals second), the coverage line, the TTFT note, the unpriced badge and the quality caveat, plus the chart data as `data-chart-data` JSON attributes. `get_compare_panels` sits behind the same `DashboardSession` extractor as the page — the fragment route is not a lesser surface. Arm labels, picker values and the coverage line render through minijinja's `.html` auto-escaping; the chart JSON goes into `data-chart-data` through the same escaped attribute path, so a value containing `<`, `"` or `&` cannot break out of the attribute or the script that reads it. Register both templates; add the nav link after Reports. Invalid selections render an inline message in the panels rather than an error page.
- **Patterns:** `get_reports` / `get_reports_panels` in `src/api/admin/reports.rs`; `reports.html` filter header; `registration_tests::every_admin_template_file_is_registered`.
- **Test scenarios:**
  - `tests/test_dashboard.rs`: a viewer session loads `/admin/compare` and sees the dimension selector, the nav link and the quality caveat text.
  - With seeded ledger rows for two models, the page's model picker lists both, and `/admin/compare/panels?dimension=model&a=X&b=Y&window=all` contains both arm labels and the request counts.
  - Tag dimension without rows renders "no data" copy, not a 500.
  - An unsafe tag key in the panels query renders the inline validation message.
  - The `/admin/compare/panels` response includes `data-chart-data` attributes containing valid JSON for the grouped bar, daily cost, and latency charts.
  - `/admin/compare/panels` without a dashboard session returns the same redirect or 401 the page returns; a session role the dashboard disallows gets the same answer as on `/admin/reports`.
  - A tag value containing `<b>x</b>`, `"quoted"` and `a&b` appears in the panels response escaped — the raw tag never appears in the body — and the `data-chart-data` JSON still parses to the original string.
  - The template registration test passes with the two new files.
- **Verification:** `cargo test --test test_dashboard`, `cargo test templates`.

### U6. Charts

- **Goal:** the three §7b charts render from the panel's embedded JSON.
- **Requirements:** R16.
- **Dependencies:** U5.
- **Files:** `templates/admin/compare_panels.html`.
- **Approach:** follow the `reports_panels.html` series code — `clearSvg`, `noData`, the shared colour constants — for a grouped bar (metrics on the x axis, A and B bars per metric, each metric normalised to its own scale with the value labelled), a two-line daily cost chart over the by-day series of both arms, and a paired p50/p95 bar chart. All three charts go through the same `noData()` path as the reports charts: the grouped bar and daily cost charts draw the "No data" label when an arm has no ledger rows, and the latency chart draws the "no latency samples" label instead of bars when the sample is zero.
- **Patterns:** `reports_panels.html` chart blocks.
- **Test scenarios:** `Test expectation: none -- client-side rendering; the panels test in U5 asserts the chart data attributes are present and well-formed JSON.`
- **Verification:** manual check in a browser against seeded data; `cargo test --test test_dashboard` for the data attributes.

### U7. CLI `report compare`

- **Goal:** the same comparison from the terminal, for scripts and for operators without a browser.
- **Requirements:** R17.
- **Dependencies:** U4.
- **Files:** `src/cli/commands.rs`, `src/cli/mod.rs`.
- **Approach:** add `ReportCommands::Compare { dimension, key, a, b, window, format }` next to `Cost`, map it onto `CompareQuery`, build `CompareSources` from settings (open the main database as `Commands::Report` already does; open `settings.storage.prompt_db_path` as the prompt database when set, otherwise reuse the main one; `CostCalculator::new_with_config(&settings.pricing)`), call `build_comparison`, and print either the JSON document or a table with one row per metric and columns A, B, delta, percent — the coverage line and caveats printed beneath.
- **Patterns:** `ReportCommands::Cost` and `report_attribution` in `src/cli/mod.rs`.
- **Test scenarios:**
  - `--format json` output parses and equals the endpoint's document for the same query.
  - Invalid dimension exits non-zero with the validation message.
  - Table output includes the latency sample count and the quality caveat.
- **Verification:** `cargo test cli`, plus a manual run against the e2e fixture database.

### U8. Client experiment guide and release notes

- **Goal:** a client application can run an experiment and read its results from the documentation alone.
- **Requirements:** R18, R19; KD1.
- **Dependencies:** U4, U5, U7 (the commands and responses it shows must exist).
- **Files:** `docs/experiments.md` (new), `README.md`, `CLAUDE.md`, `CHANGELOG.md`.
- **Approach:** the guide walks one experiment end to end, with copy-pasteable examples and no downstream application names:
  1. Design — pick the question (model A vs model B, or prompt variant A vs B on one model), an experiment name, arm names and a stopping window.
  2. Tag each request — the `attribution` body block with `tags: {"experiment": "checkout-v2", "arm": "checkout-v2:a"}` and a `correlation_id` per run, or the `X-Attribution-*` headers; the model per arm is chosen by the client in the request body; note the tag limits and that attribution never affects routing or the cache key. State plainly that the compare page partitions on one tag key, which is why the experiment name is folded into the arm value: two experiments that both used `"arm": "a"` would be merged. Send experiment traffic with `stream: false` — streamed responses record estimated or zero tokens and a placeholder latency, so a streamed arm compares placeholders, not measurements.
  3. Assign arms — a short paragraph on client-side assignment (hash of a stable id, or alternate), noting the router does not assign arms today.
  4. Retrieve — create a dedicated read-only admin for the application (`modelrouter admin create --name <app> --role viewer`; never hand an application the superadmin the CLI creates by default), keep its password in the environment, `POST /admin/api/login` with `name`/`password` to get a JWT, show `Authorization: Bearer <jwt>` with a placeholder rather than a literal token, and note the JWT expires after `auth.jwt_expiry_mins` (default 60) so the client logs in again on 401; then `GET /admin/api/compare?dimension=tag&key=arm&a=a&b=b&window=weekly` with an annotated response sample; the CLI equivalent; how `dimension=run` compares two correlation ids and `dimension=model` compares two models regardless of tags.
  5. View — `/admin/compare` on the dashboard, what each block means, and the honesty notes: latency sample vs request count, unpriced badge, no TTFT, no quality signal.
  6. Reading the result — per-request figures are the comparison; totals depend on traffic split; the page shows deltas, not significance.
  Add a README pointer beside the attribution section and the endpoint to the CLAUDE.md table; add a "this week" bullet to CHANGELOG.
- **Patterns:** README "Request cost attribution" section; CHANGELOG weekly format.
- **Test scenarios:** `Test expectation: none -- documentation; the request and response samples are taken from the U4 integration test output so they match the implementation.`
- **Verification:** every example request in the guide is exercised by `tests/test_compare.rs`; `grep -rniI --exclude-dir=target` for downstream names before commit.

---

## Verification Contract

| Check | Command | Units | Done signal |
|---|---|---|---|
| Unit and integration tests | `cargo test` | U1–U5, U7 | all pass, including the new `test_compare` and updated `test_dashboard` |
| Postgres compile | `cargo build --features postgres` | U1, U2 | builds clean |
| Postgres runtime | `cargo test --features postgres --test test_compare_postgres -- --ignored` with `MODELROUTER_TEST_POSTGRES_URL` set | U1, U2 | passes where a Postgres is provided; not run in the default environment |
| Lint | `cargo clippy --all-targets` | all | no new warnings |
| Template registration | `cargo test templates` | U5 | passes with the two new files |
| End to end | `cargo test --test test_e2e_accounting -- --ignored` | U4 | existing accounting tier still green after the ledger predicate change |
| Privacy | `grep -rniI --exclude-dir=target <name> .` for any downstream name | all | zero hits |
| Manual | load `/admin/compare` against seeded data | U5, U6 | three charts render; zero-sample arm shows dashes and the label, not a chart |

---

## Definition of Done

- `GET /admin/api/compare` and `/admin/compare` exist, share `build_comparison`, and are covered by `tests/test_compare.rs` and `tests/test_dashboard.rs` for the happy path, every 400 case, the zero-row arm, the unpriced badge and the error rate.
- No SQL statement in the comparison path interpolates a tag key; the SQLite attribution predicate binds the JSON path.
- The page and JSON carry the latency sample count, the TTFT note, the unpriced badge and the quality caveat.
- `modelrouter report compare` prints the same numbers as the endpoint.
- `docs/experiments.md` walks a client through tagging, retrieval and viewing with examples matching the tests; README, CLAUDE.md and CHANGELOG are updated.
- `cargo test`, `cargo build --features postgres` and `cargo clippy --all-targets` are clean; no migration was added.

---

## Deferred / Open Questions

### From 2026-09-03 review

- **`window=all` contradicts the bounded-query promise** — R5 / R2 (P1, whole-document peer, confidence 75)

  R2 (the endpoint contract) accepts `window=all` while R5 (the bounded-query requirement) promises every comparison is window-limited; an all-time comparison on a long-running deployment scans the whole ledger and prompts tables with the prompts date index still deferred. Either drop `all` from the compare endpoint or restate R5 to say the query is aggregate-only and the window is the operator's choice.

- **No initial-render state before arm selection; picker B can select A's value** — U5 (P1, design-lens, confidence 75)

  The page loads with no arms chosen and the plan does not say what the panels show then (empty target, prompt text, or a default pair such as the two most-used models), nor that picker B excludes the value chosen in A — a user who picks the same value twice gets the inline validation message instead of being prevented.

- **Badge arms whose pricing arrived after their traffic** — KTD8 / U3 (P2, feasibility + adversarial, confidence 75)

  The unpriced badge only fires when a model has no price now; an arm whose rows were recorded before its pricing entry existed shows $0 cost with no warning, which is exactly the "zero that is not a zero" KD3 forbids. A second trigger — tokens above zero, cost at zero, and fewer cache hits than requests in the per-model breakdown — would catch it, with badge copy that says pricing may have changed.

- **Picker navigation: HTMX swap or plain form GET** — U5 (P2, design-lens, confidence 75)

  Changing the dimension re-renders the page, but the plan leaves the mechanism open; an `hx-get` with `hx-target="body"` needs `hx-push-url` for the URL to stay bookmarkable, whereas a plain form GET gets that for free at the cost of a full reload.
