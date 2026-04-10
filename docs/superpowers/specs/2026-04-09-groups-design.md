# Groups Feature Design

## Goal

Add a Groups system that supports cost attribution across sets of users, with soft group-level spend tracking and hard per-user/per-project enforcement gates.

## Background

Currently `users` has a `group_name TEXT` column — a simple flat label with no relational structure. This feature replaces it with a proper many-to-many group membership model supporting temporal membership, priority-based attribution, and group lifecycle management.

`budget_rules` also has a `group_name TEXT` column that is used by the existing budget enforcement path. That column is **not touched by this migration** — it remains a plain string label in `budget_rules` and will be addressed when the budget management UI is built. This migration only removes `users.group_name`.

## Data Model

### Migration 014

Files: `migrations/014_groups.sql` and `migrations/postgres/014_groups.sql`

**SQLite** (`migrations/014_groups.sql`):
```sql
-- Recreate users table without group_name (preserve all other columns)
CREATE TABLE users_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    email TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    spend_reset_at TEXT,
    metadata TEXT NOT NULL DEFAULT '{}'
);
INSERT INTO users_new SELECT id, name, email, enabled, created_at, spend_reset_at, metadata FROM users;
DROP TABLE users;
ALTER TABLE users_new RENAME TO users;

CREATE TABLE groups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    priority INTEGER NOT NULL DEFAULT 0,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE group_memberships (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id INTEGER NOT NULL REFERENCES groups(id),
    user_id INTEGER NOT NULL REFERENCES users(id),
    joined_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    disabled_at TEXT
);
```

**Postgres** (`migrations/postgres/014_groups.sql`):
```sql
ALTER TABLE users DROP COLUMN IF EXISTS group_name;

CREATE TABLE IF NOT EXISTS groups (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    priority BIGINT NOT NULL DEFAULT 0,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE TABLE IF NOT EXISTS group_memberships (
    id BIGSERIAL PRIMARY KEY,
    group_id BIGINT NOT NULL REFERENCES groups(id),
    user_id BIGINT NOT NULL REFERENCES users(id),
    joined_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    disabled_at TEXT
);
```

### Schema Notes

- No `UNIQUE` constraint on `(group_id, user_id)` in `group_memberships` — a user may be re-added after removal, producing a new row. "One active membership per pair" is enforced at the application layer.
- `budget_rules.group_name` is unchanged — it is a plain string label, not a FK, and is out of scope for this feature.
- The `users.group_name` drop removes the column from `src/db/models.rs` `User` and `NewUser` structs and all their callers (dashboard, CLI, REST routes). These must all be updated as part of this migration task.
- **Breaking REST API change:** `src/api/admin/routes.rs` has a `UserResponse` struct with a `group_name: Option<String>` field that is serialized in `GET /admin/users` responses. Removing `group_name` from `User` means this field must also be removed from `UserResponse`. This is an intentional breaking change to the admin REST API — implementers must remove the field, not null-preserve it.

### Cost Attribution

Spend is attributed to a group by joining `cost_ledger` → `group_memberships` on `user_id`, filtered to records where `cost_ledger.created_at >= group_memberships.joined_at AND (group_memberships.disabled_at IS NULL OR cost_ledger.created_at < group_memberships.disabled_at)`.

When a user belongs to multiple groups, **all of their spend is attributed to the single highest-priority group they are an active member of** (highest `priority` integer wins). If two groups have equal priority, the one with the lower `id` (created first) wins. This is a reporting-only calculation — no splitting.

## Models

New structs in `src/db/models.rs`:

```rust
pub struct Group {
    pub id: i64,
    pub name: String,
    pub priority: i64,
    pub enabled: bool,
    pub created_at: String,
}

pub struct GroupMembership {
    pub id: i64,
    pub group_id: i64,
    pub user_id: i64,
    pub user_name: String,  // aliased from JOIN with users table
    pub joined_at: String,
    pub disabled_at: Option<String>,
}
```

`GroupMembership` derives `sqlx::FromRow`. All queries that fetch memberships must JOIN `users` and alias the column as `user_name` (e.g., `SELECT gm.*, u.name AS user_name FROM group_memberships gm JOIN users u ON u.id = gm.user_id`). The `user_name` field is never nullable in the query result because `user_id` is a FK.

Remove `group_name` from `User` and `NewUser` structs.

## Backend

### Repository Layer

New files:
- `src/db/repositories/groups.rs` — trait definitions
- `src/db/sqlite/groups.rs` — SQLite implementation
- `src/db/postgres/groups.rs` — Postgres implementation

Functions:

```rust
async fn list_groups(&self) -> Result<Vec<Group>>;
async fn get_group(&self, id: i64) -> Result<Option<Group>>;
async fn find_group_by_name(&self, name: &str) -> Result<Option<Group>>;
async fn create_group(&self, name: &str, priority: i64) -> Result<Group>;
async fn set_group_enabled(&self, id: i64, enabled: bool) -> Result<()>;
async fn list_memberships(&self, group_id: i64) -> Result<Vec<GroupMembership>>;
async fn find_active_membership(&self, group_id: i64, user_id: i64) -> Result<Option<GroupMembership>>;
async fn add_member(&self, group_id: i64, user_id: i64) -> Result<GroupMembership>;
async fn disable_membership(&self, membership_id: i64) -> Result<()>;
async fn disable_all_memberships(&self, group_id: i64) -> Result<()>;
```

**Transaction requirement:** `set_group_enabled(id, false)` and `disable_all_memberships(group_id)` must be wrapped in a single database transaction to prevent partial state on failure.

**Priority updates:** Changing a group's priority after creation is out of scope. `priority` is set at creation time only.

### Validation (handler layer)

- **Duplicate group name:** Call `find_group_by_name` before insert; return HTTP 409 with inline error HTML if found.
- **Add member to disabled group:** Handler calls `get_group` first; if `!group.enabled`, return HTTP 400 with inline error HTML. The `add_member` repository function does not perform this check.
- **Re-add a previously-disabled member:** Allowed. Handler calls `find_active_membership` — if an active membership exists, return error "user already a member". If no active membership, insert a new row (old disabled row preserved for historical attribution).
- **Disable already-disabled group:** `set_group_enabled(id, false)` is idempotent; `disable_all_memberships` with no active rows is a no-op.

### HTTP Routes

All mutations require `SuperDashboardSession`. Reads require `DashboardSession`.

| Method | Path | Success response | Error response |
|--------|------|-----------------|----------------|
| GET | `/admin/groups` | Full page HTML | — |
| POST | `/admin/groups` | Re-rendered group card partial (`#group-card-{id}`) injected via HTMX OOB or template+script | HTTP 409 inline error in form result div |
| POST | `/admin/groups/:id/enable` | Re-rendered group card (`outerHTML` swap of `#group-card-{id}`) | HTTP 400 inline error |
| POST | `/admin/groups/:id/disable` | Re-rendered group card (`outerHTML` swap) | HTTP 400 inline error |
| POST | `/admin/groups/:id/members` | Re-rendered group card (`outerHTML` swap) | HTTP 400/409 inline error |
| POST | `/admin/groups/:id/members/:uid/disable` | Re-rendered group card (`outerHTML` swap) | HTTP 400 inline error |

Each mutation endpoint returns HTTP 200 with the full re-rendered group card HTML on success. Buttons use `hx-target="#group-card-{id}" hx-swap="outerHTML"`. Error responses return HTTP 4xx with an inline error partial; the HTMX `hx-target` on the button points to the card, so errors replace the card with an error card (or the card re-renders with an error message inlined).

Handler file: `src/api/admin/groups.rs`
Routes registered in `src/api/admin/mod.rs` alongside existing admin routes.

## Admin UI

Single page at `/admin/groups` (`templates/admin/groups.html`).

### Create Group Form

Fields: Name (text, required), Priority (integer input, default 0).

No Members field at creation time — add members via the group card after creation. This keeps the `POST /admin/groups` handler to a single atomic insert with no partial-failure risk.

HTMX: `hx-post="/admin/groups" hx-target="#groups-list" hx-swap="afterbegin"` to prepend new card. On duplicate name: inline error in `#group-form-result` div.

### Group List (`<div id="groups-list">`)

Each group: `<div id="group-card-{id}">`.

**Enabled group card:**
- Name, priority badge, "Enabled" status badge
- Member table: User | Joined | Status (Active / Disabled `YYYY-MM-DD HH:MM:SS`)
- Add Member: dropdown of users not currently active in this group + Add button (`hx-post="/admin/groups/{id}/members"`)
- Disable Member button per active member (`hx-post="/admin/groups/{id}/members/{uid}/disable"`)
- Disable Group button (`hx-post="/admin/groups/{id}/disable"` with `hx-confirm`)

**Disabled group card:**
- Name, priority badge, "Disabled" status badge, light gray background
- Historical member list (read-only, all members with their `disabled_at`)
- Re-enable Group button (`hx-post="/admin/groups/{id}/enable"`)
- No Add Member or Disable Member controls

Disabled group cards appear below enabled groups. Re-enabling a group returns an enabled card with all memberships still disabled — admin must explicitly add members.

## Navigation

Add "Groups" link to the admin sidebar in `templates/admin/base.html`, positioned **after API Keys** in the existing nav order.

## Testing

- Unit tests in `tests/` or alongside repository implementations:
  - Create group, duplicate name rejected
  - Add member, active duplicate rejected
  - Re-add previously-disabled member succeeds (new row)
  - Disable membership sets `disabled_at`
  - Disable group: transaction sets `enabled=0` + disables all active memberships
  - Re-enable group: `enabled=1`, memberships remain disabled
  - Add member to disabled group rejected at handler layer
- `cargo build --features postgres` must pass (postgres migration + model changes)
- Postgres migration tested as fresh install from `001` to verify no schema drift

## Out of Scope

- Budget enforcement (separate feature after Groups)
- CLI subcommands for group management
- Group renaming or priority editing after creation
- Priority reordering via drag-and-drop
- REST API endpoints for groups (dashboard-only)
