# Budget Management Admin Page Design

## Goal

Add a Budget Management admin page that lets operators create, edit, and delete budget rules scoped to global (org-wide), project, user, and group (soft targets). Supports both monthly-reset and fixed date-range ("total") windows per scope.

## Background

The existing `budget_rules` table enforces per-user and group-based spend limits. This feature extends it with project-scoped rules, total (date-range) window support, and a full admin UI for managing all rules. Group-scoped rules become soft targets — tracked but not enforced.

## Data Model

### Migration 015

**SQLite** (`migrations/015_budgets.sql`):
```sql
ALTER TABLE budget_rules ADD COLUMN project TEXT;
ALTER TABLE budget_rules ADD COLUMN window_start TEXT;
ALTER TABLE budget_rules ADD COLUMN window_end TEXT;
```

**Postgres** (`migrations/postgres/015_budgets.sql`):
```sql
ALTER TABLE budget_rules ADD COLUMN project TEXT;
ALTER TABLE budget_rules ADD COLUMN window_start TEXT;
ALTER TABLE budget_rules ADD COLUMN window_end TEXT;
```

### Scope Determination

Scope is determined by which discriminator field is non-null:

| Scope | user_id | group_name | project | api_key_id |
|-------|---------|------------|---------|------------|
| Global | null | null | null | null |
| Project | null | null | set | null |
| User | set | null | null | null |
| Group (soft target) | null | set | null | null |

`api_key_id` and `tag` columns are unchanged and out of scope for this feature.

### Window Types

- `window = "monthly"` — resets each calendar month; `window_start` and `window_end` are null
- `window = "total"` — fixed date range; `window_start` and `window_end` are required (ISO 8601 date strings, e.g. `"2026-01-01"`)
- `window = "target"` — group-scoped rules only; no date range, not enforced, stores `limit_usd` as an informational spend target

A scope entity (global, project, user) can have at most one `monthly` rule and one `total` rule, enforced at the application layer. A group entity has at most one `target` rule. If both monthly and total are set for a scope, they are enforced independently — a request is blocked when either limit is reached.

**Uniqueness note:** At most one monthly and one total rule per scope is enforced via pre-insert query check at the application layer (no DB unique index). A unique index is not feasible for global scope because SQLite treats each NULL as distinct, preventing `UNIQUE (user_id, project, window)` from blocking duplicate global rows. The TOCTOU race window is accepted as an acceptable limitation for low-traffic admin operations.

## Models

Update in `src/db/models.rs`:

```rust
pub struct BudgetRule {
    // existing fields ...
    pub project: Option<String>,
    pub window_start: Option<String>,
    pub window_end: Option<String>,
}

pub struct NewBudgetRule {
    // existing fields ...
    pub project: Option<String>,
    pub window_start: Option<String>,
    pub window_end: Option<String>,
}

/// Fields editable after creation. Scope (user_id, group_name, project) and
/// window type are immutable — delete and recreate to change them.
pub struct UpdateBudgetRule {
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<i64>,
    pub model_allow: Option<String>,
    pub model_deny: Option<String>,
    pub rate_rpm: Option<i64>,
    pub max_concurrent: Option<i64>,
    pub window_start: Option<String>,  // only for window="total" rules
    pub window_end: Option<String>,    // only for window="total" rules
}
```

New enum in `src/db/models.rs`:

```rust
pub enum BudgetScope {
    Global,
    Project(String),
    User(i64),
    Group(String),
}
```

## Backend

### Repository Layer

Add to the existing `BudgetRepository` trait (`src/db/repositories/budgets.rs`):

```rust
async fn list_for_scope(&self, scope: &BudgetScope) -> anyhow::Result<Vec<BudgetRule>>;
async fn update(&self, id: i64, update: &UpdateBudgetRule) -> anyhow::Result<BudgetRule>;
```

Existing methods reused as-is:
- `create(&self, rule: NewBudgetRule)` — for all creates
- `delete(&self, id: i64)` — for all deletes (from the handler; enforcement path uses the existing per-scope list methods)
- `list_all(&self)` — for the page GET to load all rules at once

Add to the existing `CostRepository` trait (`src/db/repositories/costs.rs`):

```rust
/// Sum spend for a user between two ISO 8601 timestamps (inclusive start, exclusive end).
async fn sum_for_user_between(&self, user_id: i64, start: &str, end: &str) -> anyhow::Result<f64>;
/// Sum spend for a project since a timestamp (inclusive).
/// Project spend = all cost_ledger rows where cost_ledger.project = project_name.
async fn sum_for_project_since(&self, project: &str, since: &str) -> anyhow::Result<f64>;
/// Sum spend for a project between two ISO 8601 timestamps (inclusive start, exclusive end).
/// Project spend = all cost_ledger rows where cost_ledger.project = project_name.
async fn sum_for_project_between(&self, project: &str, start: &str, end: &str) -> anyhow::Result<f64>;
/// Sum all spend (global) since a timestamp (inclusive).
async fn sum_global_since(&self, since: &str) -> anyhow::Result<f64>;
/// Sum all spend (global) between two timestamps (inclusive start, exclusive end).
async fn sum_global_between(&self, start: &str, end: &str) -> anyhow::Result<f64>;
```

`cost_ledger.project` is already denormalized at write time, so project spend queries filter on that column directly.

Implement all new methods in `src/db/sqlite/costs.rs` and `src/db/postgres/costs.rs`.

Implement `list_for_scope`, `update_budget_rule`, and `delete_budget_rule` in `src/db/sqlite/budgets.rs` and `src/db/postgres/budgets.rs`.

### Validation (handler layer)

- **Duplicate window for scope:** Before creating, call `list_for_scope` and check existing rules. If a rule with the same `window` type already exists for that scope, return HTTP 409 with inline error.
- **Total window requires dates:** If `window = "total"`, both `window_start` and `window_end` must be present and `window_start < window_end`; return HTTP 400 otherwise.
- **Group rules are targets only:** Group-scoped rules use `window = "target"` and store `limit_usd` only; `limit_tokens`, `model_allow`, `model_deny`, `rate_rpm`, `max_concurrent` are null and not shown in the group UI. The "total window requires dates" validation does not apply to `window = "target"`.
- **Window type immutable on edit:** The edit form does not include a window-type selector. `UpdateBudgetRule` does not include a `window` field. Operators delete and recreate to change window type.
- **Group deletion orphan handling:** `group_memberships.group_id` has a FK to `groups.id`, but `budget_rules.group_name` is a plain string (not a FK). Group deletion does not cascade to budget rules. The Groups tab handler fetches all rules where `group_name IS NOT NULL` and cross-references against the `groups` table; orphaned rules (no matching group) are shown with a "Group not found" label and a Delete button only.

### Enforcement Changes (`src/router/policy.rs`)

- **Project-scope:** When evaluating budget rules for a request, check rules where `budget_rules.project = api_key.project`. Use `sum_for_project_since` (monthly) or `sum_for_project_between` (total) from `CostRepository`.
- **Global-scope:** Check rules where all discriminator fields are null. Use `sum_global_since` (monthly) or `sum_global_between` (total).
- **User-scope:** Existing monthly enforcement uses `sum_for_user_since`. Add `sum_for_user_between` for total-window user rules.
- **Skip group rules:** Rules where `group_name IS NOT NULL` are skipped during enforcement — they are informational only.
- **Enforcement precedence:** All applicable rules (user + project + global) are checked independently. Any limit hit blocks the request.

### HTTP Routes

All mutations require `SuperDashboardSession`. Reads require `DashboardSession`.

| Method | Path | Success response | Error response |
|--------|------|-----------------|----------------|
| GET | `/admin/budgets` | Full page HTML | — |
| POST | `/admin/budgets` | Re-rendered scope card partial | HTTP 400/409 inline error |
| POST | `/admin/budgets/:id/edit` | Re-rendered scope card partial | HTTP 400 inline error |
| POST | `/admin/budgets/:id/delete` | Re-rendered scope card partial | HTTP 400 inline error |

Handler file: `src/api/admin/budgets.rs`
Routes registered in `src/api/admin/mod.rs`.

## Admin UI

Single page at `/admin/budgets` (`templates/admin/budgets.html`).

### Tab Layout

Four tabs: **Global | Projects | Users | Groups**

Tab switching is client-side only (CSS show/hide on `<div id="tab-global">` etc.), no server round-trip.

### Global Tab

One card (`<div id="budget-card-global">`). Shows:
- Current monthly rule row (limit_usd, limit_tokens, model_allow/deny, rate_rpm, max_concurrent) with Edit and Delete buttons
- Current total rule row with date range, same controls
- "Add Monthly Limit" button if no monthly rule exists
- "Add Total Limit" button (with date inputs) if no total rule exists

Edit toggles the rule row to an inline form (no window-type selector — type is immutable); submitting re-renders the whole card via `outerHTML` swap.

### Projects Tab

One card per distinct project name (`<div id="budget-card-project-{name}">`). Projects are sourced from the union of distinct non-null `api_keys.project` values and distinct non-null `budget_rules.project` values — ensuring that rules for projects whose keys have been deleted remain visible and deletable. Each card shows monthly and total rules for that project with the same controls as the Global card. A "New Project Budget" form at the top of the tab lets operators enter a project name to create the first rule for a new project.

### Users Tab

One card per user (`<div id="budget-card-user-{id}">`). Users sourced from the `users` table (all users, enabled or not). Same monthly/total rule display and controls.

### Groups Tab

One card per group (`<div id="budget-card-group-{name}">`). Groups sourced from the `groups` table. Each card shows a single `limit_usd` target (stored as `window = "target"`, no date range collected). Labeled "Target" not "Budget". No enforcement indicator. Only `limit_usd` is shown and editable. Orphaned budget rules (group deleted but rule remains) show a "Group not found" label with Delete only.

### Navigation

Add "Budgets" link to the admin sidebar in `templates/admin/base.html`, positioned after **Groups** in the existing nav order.

## Testing

- Create global monthly rule; creating second monthly rule returns 409
- Create global total rule with valid date range; without dates returns 400
- Create project rule; create user rule; both enforce independently
- Global rule enforcement: sum_global_since and sum_global_between called correctly
- Project rule enforcement: sum_for_project_since and sum_for_project_between called correctly
- Group rule stored with window="target" but not enforced (enforcement path skips group-scoped rows)
- Update rule: limit_usd changes, window type unchanged, other fields preserved
- Delete rule: row removed, card re-renders without the rule
- `window="total"` enforcement: spend within range blocked, spend outside range unaffected
- Orphaned group budget rule (group deleted): shows "Group not found" label, Delete button works
- `cargo build --features postgres` passes after migration

## Out of Scope

- Budget enforcement for group-scoped rules
- CLI subcommands for budget management
- REST API endpoints for budget rules (dashboard-only)
- Tag-scoped or api_key_id-scoped rules via this UI (existing rows still enforced, just not editable here)
- Budget alerting or notifications
