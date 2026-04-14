# Reports Page — Design Spec
**Date:** 2026-04-13  
**Status:** Approved

## Summary

Add a `/admin/reports` page to the modelrouter admin dashboard with multi-dimensional spend detail panels and a burndown chart showing cumulative spend vs. budget limit. Uses the existing HTMX + minijinja pattern with D3.js (vendored) for charting.

---

## Routes

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/admin/reports` | Full page shell — filter bar + empty `#panels` div |
| `GET` | `/admin/reports/panels` | HTMX partial — renders panel HTML fragment with embedded JSON |

The filter bar uses `hx-get="/admin/reports/panels"` `hx-target="#panels"` `hx-trigger="change, load"` so the panels load on page load and refresh on any filter change without a full page reload.

---

## Filter Bar

Five `<select>` controls in a horizontal flex row at the top of the page:

| Control | Options | Default |
|---------|---------|---------|
| User | All users + each user name | All users |
| Project | All projects + distinct `project` values from `cost_ledger` | All projects |
| Model | All models + distinct `model` values from `cost_ledger` | All models |
| Group | All groups + each group name | All groups |
| Window | Daily / Weekly / Monthly | Monthly |

All five are passed as query params to `/admin/reports/panels`.

---

## Panel Layout

Six panels in a 2-column CSS grid using the existing `.stat-card` style:

| Panel | Type | Content |
|-------|------|---------|
| Spend by User | Table | user, cost (USD), tokens in, tokens out, requests |
| Spend by Model | Table | model, cost (USD), tokens in, tokens out, requests |
| Spend by Project | Table | project, cost (USD), tokens in, tokens out, requests |
| Top Models | D3 horizontal bar chart | Cost per model, descending |
| Token Usage | D3 stacked bar chart | tokens_in vs tokens_out per user |
| Burndown | D3 line chart | Cumulative daily spend vs. budget limit line per user/group |

D3 charts receive data via `data-*` attributes on their container `<div>` elements (JSON-encoded). A `<script>` block at the bottom of `reports_panels.html` initializes all charts and re-initializes on `htmx:afterSwap`.

---

## New Files

- `src/api/admin/reports.rs` — handler functions (`get_reports`, `get_reports_panels`)
- `templates/admin/reports.html` — full page template (extends `base.html`)
- `templates/admin/reports_panels.html` — panels fragment (no base extension)
- `src/api/admin/d3.min.js` — vendored D3 v7, served at `/static/d3.js`

Existing files modified:
- `src/api/admin/mod.rs` — add `pub mod reports`
- `src/api/admin/templates.rs` — register `reports.html` and `reports_panels.html`
- `src/api/app.rs` — add two new routes
- `templates/admin/base.html` — add "Reports" nav link + `<script src="/static/d3.js">`
- `src/db/repositories/costs.rs` — add two new trait methods
- `src/db/sqlite/costs.rs` — implement new trait methods
- `src/db/postgres/costs.rs` — implement new trait methods

---

## New DB Methods

### `list_daily_spend`

```rust
async fn list_daily_spend(
    user_id: Option<i64>,
    project: Option<&str>,
    model: Option<&str>,
    group_user_ids: Option<&[i64]>,  // pre-resolved from group name
    start: &str,
    end: &str,
) -> anyhow::Result<Vec<(String, f64)>>
```

Returns `(date, cost_usd)` pairs grouped by calendar day (`strftime('%Y-%m-%d', created_at)` in SQLite). Filters are ANDed; `None` = no filter. Group filter uses `user_id IN (...)` with pre-resolved member IDs.

### `summarize_by_model`

```rust
async fn summarize_by_model(
    user_id: Option<i64>,
    project: Option<&str>,
    model_filter: Option<&str>,
    group_user_ids: Option<&[i64]>,
    since: &str,
) -> anyhow::Result<Vec<ModelSummaryRow>>
```

`ModelSummaryRow`: `{ model: String, total_cost_usd: f64, tokens_in: i64, tokens_out: i64, request_count: i64 }`

Groups by `model` column in `cost_ledger`.

---

## Burndown Logic (Handler-Side)

1. Resolve window `start` and `end` timestamps from the selected window (daily/weekly/monthly)
2. Call `list_daily_spend(...)` for the window
3. Compute cumulative sum: `[(date, cumulative_usd)]`
4. If a user or group is selected, look up their active budget rule for the window → `limit_usd`
5. Pass `{ series: [(date, cumul)], limit: f64|null }` as JSON to the template

If no budget rule exists, the burndown chart renders with no limit line (just the cumulative spend line).

---

## Group Filter Resolution

When a group name filter is active:
1. Handler calls `GroupRepository::find_by_name` → group id
2. Handler calls `GroupRepository::list_members(group_id)` → `Vec<i64>` of user IDs
3. Passes `group_user_ids: Some(&ids)` to both new DB methods

---

## D3 Chart Initialization

```js
// In reports_panels.html <script> block
function initCharts() {
    initTopModelsChart();
    initTokenUsageChart();
    initBurndownChart();
}
document.addEventListener('DOMContentLoaded', initCharts);
document.addEventListener('htmx:afterSwap', function(e) {
    if (e.detail.target.id === 'panels') initCharts();
});
```

Each chart function reads its data from `data-chart-data` on the container element, clears any existing SVG, then draws.

---

## Constraints

- No new migration needed — all queries use existing `cost_ledger`, `group_memberships`, `budget_rules` tables
- Auth: both routes require `DashboardSession` (admin or superadmin)
- D3 vendored at `src/api/admin/d3.min.js`, served via existing static file handler at `/static/d3.js`
- All filters are optional; the page is fully usable with no filters selected
