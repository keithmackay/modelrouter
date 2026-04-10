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

A scope entity can have at most one monthly rule and one total rule, enforced at the application layer. If both are set, they are enforced independently — a user is blocked when either limit is reached.

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

pub struct UpdateBudgetRule {
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<i64>,
    pub model_allow: Option<String>,
    pub model_deny: Option<String>,
    pub rate_rpm: Option<i64>,
    pub max_concurrent: Option<i64>,
    pub window_start: Option<String>,
    pub window_end: Option<String>,
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
async fn list_budget_rules_by_scope(&self, scope: &BudgetScope) -> Result<Vec<BudgetRule>>;
async fn list_all_budget_rules(&self) -> Result<Vec<BudgetRule>>;
async fn update_budget_rule(&self, id: i64, update: &UpdateBudgetRule) -> Result<BudgetRule>;
async fn delete_budget_rule(&self, id: i64) -> Result<()>;
```

The existing `create_budget_rule` method is reused for all creates.

Implement in `src/db/sqlite/budgets.rs` and `src/db/postgres/budgets.rs`.

### Validation (handler layer)

- **Duplicate window for scope:** Before creating, query existing rules for the scope. If a rule with the same `window` type already exists, return HTTP 409 with inline error.
- **Total window requires dates:** If `window = "total"`, both `window_start` and `window_end` must be present and `window_start < window_end`; return HTTP 400 otherwise.
- **Group rules are targets only:** Group-scoped rules store `limit_usd` only; `limit_tokens`, `model_allow`, `model_deny`, `rate_rpm`, `max_concurrent` are ignored and not shown in the group UI.

### Enforcement Changes (`src/router/policy.rs`)

- Add project-scope budget check: match `budget_rules.project` against the `project` field of the API key used in the request.
- Add total-window check: for `window = "total"` rules, sum spend in `[window_start, window_end)` and compare to `limit_usd`.
- Skip enforcement for group-scoped rules (`group_name IS NOT NULL`) — they are informational only.
- Enforcement precedence: all applicable rules are checked independently. Any limit hit blocks the request.

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

Edit toggles the rule row to an inline form; submitting re-renders the whole card via `outerHTML` swap.

### Projects Tab

One card per distinct project name (`<div id="budget-card-project-{name}">`). Projects are sourced from distinct non-null `api_keys.project` values. Each card shows monthly and total rules for that project with the same controls as the Global card. A "New Project Budget" form at the top of the tab lets operators enter a project name to create the first rule for a new project.

### Users Tab

One card per user (`<div id="budget-card-user-{id}">`). Users sourced from the `users` table (all users, enabled or not). Same monthly/total rule display and controls.

### Groups Tab

One card per group (`<div id="budget-card-group-{name}">`). Groups sourced from the `groups` table. Each card shows a single `limit_usd` target (no window type selector — always "total" interpreted as the group's spend target). Labeled "Target" not "Budget". No enforcement indicator. Only `limit_usd` is shown and editable.

### Navigation

Add "Budgets" link to the admin sidebar in `templates/admin/base.html`, positioned after **Groups** in the existing nav order.

## Testing

- Create global monthly rule; creating second monthly rule returns 409
- Create global total rule with valid date range; without dates returns 400
- Create project rule; create user rule; both enforce independently
- Group rule stored but not enforced (enforcement path skips group-scoped rows)
- Update rule: limit_usd changes, other fields preserved
- Delete rule: row removed, card re-renders without the rule
- `window="total"` enforcement: spend within range blocked, spend outside range unaffected
- `cargo build --features postgres` passes after migration

## Out of Scope

- Budget enforcement for group-scoped rules
- CLI subcommands for budget management
- REST API endpoints for budget rules (dashboard-only)
- Tag-scoped or api_key_id-scoped rules via this UI (existing rows still enforced, just not editable here)
- Budget alerting or notifications
