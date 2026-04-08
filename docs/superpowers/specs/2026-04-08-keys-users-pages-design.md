# modelrouter — Keys & Users Admin Pages

_Written: 2026-04-08_

---

## Overview

Revamp the admin dashboard to properly separate **key management** from **user management**, and remove the legacy single-key-per-user architecture. After this change:

- All API keys live in the `api_keys` table. The legacy `users.api_key` / `users.api_key_old` / `users.api_key_old_expires_at` columns are dropped.
- Users can be created without a key. Keys are always created explicitly and associated with an existing (or auto-created) user.
- The admin dashboard gains a **Keys** page (`/admin/keys`) and a repurposed **Users** page (`/admin/users`), both linked from the nav bar.

---

## Schema Changes (one migration, two files)

### `migrations/012_keys_users_refactor.sql` (SQLite)

```sql
-- Drop legacy key columns from users
ALTER TABLE users DROP COLUMN api_key;
ALTER TABLE users DROP COLUMN api_key_old;
ALTER TABLE users DROP COLUMN api_key_old_expires_at;

-- Add email field for future welcome-email feature
ALTER TABLE users ADD COLUMN email TEXT;
```

### `migrations/postgres/012_keys_users_refactor.sql` (Postgres)

```sql
-- Drop legacy key columns from users
ALTER TABLE users DROP COLUMN IF EXISTS api_key;
ALTER TABLE users DROP COLUMN IF EXISTS api_key_old;
ALTER TABLE users DROP COLUMN IF EXISTS api_key_old_expires_at;

-- Add email field for future welcome-email feature
ALTER TABLE users ADD COLUMN IF NOT EXISTS email TEXT;
```

The `api_keys` table is unchanged. The `group_name` and `enabled` columns on `users` are unchanged — `users.enabled` already exists and is not added in this migration.

---

## Modified Files

| File | Change |
|---|---|
| `migrations/012_keys_users_refactor.sql` | New — SQLite migration |
| `migrations/postgres/012_keys_users_refactor.sql` | New — Postgres migration |
| `src/db/models.rs` | Update `User`, `NewUser` structs |
| `src/db/repositories/users.rs` | Remove 3 trait methods |
| `src/db/repositories/api_keys.rs` | Add 3 trait methods |
| `src/db/sqlite/users.rs` | Update `UserRow` struct, `From` impl, all SELECT queries; remove 3 method impls |
| `src/db/sqlite/api_keys.rs` | Implement 3 new trait methods |
| `src/db/postgres/users.rs` | Same as SQLite counterpart |
| `src/db/postgres/api_keys.rs` | Implement 3 new trait methods |
| `src/api/auth.rs` | Remove legacy fallback auth path |
| `src/api/admin/routes.rs` | Update `create_user`; delete `rotate_user_key` |
| `src/api/admin/dashboard.rs` | Replace/update/delete handlers; add Keys page handlers |
| `templates/admin/base.html` | Update nav links |
| `templates/admin/users.html` | Rewrite for user-only management |
| `templates/admin/keys.html` | New — Keys page template |

---

## Model & Repository Changes

### `src/db/models.rs`

`User` struct: remove `api_key`, `api_key_old`, `api_key_old_expires_at`. Add `email: Option<String>`.

`NewUser` struct: remove `api_key_hash`. Add `email: Option<String>`. Final shape:
```rust
pub struct NewUser {
    pub name: String,
    pub group_name: Option<String>,
    pub email: Option<String>,
}
```

### `src/db/repositories/users.rs` — remove from trait

- `find_by_api_key` — deleted (only used by legacy auth path)
- `rotate_key` — deleted (key rotation moves to `api_keys`)
- `expire_old_keys` — deleted (was expiring `api_key_old`; no longer needed)

### `src/db/repositories/api_keys.rs` — add to trait

```rust
async fn list_all_api_keys(&self) -> anyhow::Result<Vec<ApiKey>>;
async fn set_key_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()>;
async fn disable_all_keys_for_user(&self, user_id: i64) -> anyhow::Result<()>;
```

The existing `revoke_api_key(id)` method is kept on the trait — it is semantically equivalent to `set_key_enabled(id, false)` and is preserved for backward compatibility with the existing REST endpoint `POST /admin/api/keys/:id/revoke`. New dashboard handlers use `set_key_enabled` to allow both disable and re-enable. `revoke_api_key` is not removed.

Implement all three new methods for both `SqliteDb` and `PostgresDb`.

### `src/db/sqlite/users.rs` and `src/db/postgres/users.rs`

The SQLite implementation uses a private `UserRow` intermediate struct (with explicit column fields) and a `From<UserRow> for User` conversion. Both must be updated:

1. Remove `api_key`, `api_key_old`, `api_key_old_expires_at` from the `UserRow` struct
2. Remove those fields from the `From<UserRow> for User` implementation
3. Remove the dropped columns from all SELECT queries (list, find_by_id, find_by_name, create)
4. Remove implementations of `find_by_api_key`, `rotate_key`, `expire_old_keys`
5. Add `email` to `UserRow`, the `From` impl, and all SELECT/INSERT queries

### `src/api/auth.rs`

Remove the legacy fallback path (step 2: `find_by_api_key`). Auth becomes a single code path through `api_keys`. The `find_by_api_key` call site is deleted entirely.

### `src/api/admin/routes.rs`

- `create_user`: update to use new `NewUser { name, group_name, email }` (no `api_key_hash`)
- `rotate_user_key`: delete — rotation is now handled by the dashboard Keys page handler. The REST route `POST /admin/api/users/:id/rotate-key` is removed from `app.rs`.

---

## Keys Page (`/admin/keys`)

### Routes

| Method | Path | Handler | Auth |
|---|---|---|---|
| GET | `/admin/keys` | `get_keys` | DashboardSession |
| POST | `/admin/keys` | `post_create_key` | SuperDashboardSession |
| POST | `/admin/keys/:id/disable` | `post_disable_key` | SuperDashboardSession |
| POST | `/admin/keys/:id/rotate` | `post_rotate_key` | SuperDashboardSession |

### Data loaded for GET

- All rows from `api_keys` joined with `users.name` — sorted: enabled keys first (by `created_at` DESC), disabled after (by `created_at` DESC)
- All distinct `users.name` values → for User `<datalist>`
- All distinct non-null `api_keys.project` values → for Project `<datalist>`

### Create Key Form

```
[ User (datalist)* ] [ Project (datalist) ] [ Label ] [ Email (disabled) ] [ Create Key ]
```

- **User** — required. `<input list="user-list">` backed by `<datalist id="user-list">` of all existing user names. If the submitted name does not match any existing user, a new `users` row is created first via `UserRepository::create`. The `users.name` column has a UNIQUE constraint; if a concurrent request creates the same name simultaneously, fall back to `find_by_name` to retrieve the existing user.
- **Project** — optional. `<input list="project-list">` backed by `<datalist id="project-list">` of all distinct existing project values.
- **Label** — optional free text (e.g. "laptop", "CI", "claude-code").
- **Email** — `<input disabled placeholder="Coming soon — will email key + .envrc instructions to user">`. The field value is accepted but not acted on. A `// TODO: send welcome email` stub comment marks the future hook point in the handler.

On success: HTMX prepends new row to table. Raw key shown once in the Actions cell in green. Form resets via `hx-on::after-request="this.reset()"`.

### Key Table Columns

| Column | Source |
|---|---|
| User | `users.name` (via JOIN) |
| Project | `api_keys.project` or — |
| Label | `api_keys.label` or — |
| Status | Enabled / Disabled tag |
| Created | `api_keys.created_at` |
| Actions | Disable / Rotate buttons |

Re-enabling individual keys from the dashboard UI is intentionally out of scope for this iteration — if a key was disabled in error, the operator should rotate it to get a fresh key. A `POST /admin/keys/:id/enable` route can be added in a future pass.

### Disable Key (`POST /admin/keys/:id/disable`)

Calls `set_key_enabled(id, false)`. Returns updated row fragment (HTMX outerHTML swap). Row stays in current position; sort updates on next page load.

### Rotate Key (`POST /admin/keys/:id/rotate`)

1. Fetch existing key row (get `user_id`, `project`, `label`)
2. Call `set_key_enabled(id, false)` on old key
3. Generate new token (`mr-<uuid>` without hyphens), hash it
4. Call `create_api_key` with same `user_id`, `project`, `label`
5. Return new row fragment showing new token once in green

### Audit Log — Keys Page

All mutating handlers write a `NewAuditLogEntry` via `AuditRepository::create`:

| Action | `action` string | `target` | `after_json` |
|---|---|---|---|
| Create key | `"key.create"` | `"key:<id>"` | `{"user_id":…,"project":…,"label":…}` |
| Disable key | `"key.disable"` | `"key:<id>"` | `{"enabled":false}` |
| Rotate key | `"key.rotate"` | `"key:<new_id>"` | `{"user_id":…,"replaced_key_id":…}` |

`actor_id` = session JWT `sub`, `actor_name` = session JWT `name`.

---

## Users Page (`/admin/users`)

### Routes

| Method | Path | Handler | Auth |
|---|---|---|---|
| GET | `/admin/users` | `get_users` | DashboardSession |
| POST | `/admin/users` | `post_create_user` | SuperDashboardSession |
| POST | `/admin/users/:id/disable` | `post_disable_user` | SuperDashboardSession |
| POST | `/admin/users/:id/enable` | `post_enable_user` | SuperDashboardSession |

### Data loaded for GET

All rows from `users`. Sorted: enabled users first (by `created_at` DESC), disabled after.

The `group_name` column exists on users but is intentionally omitted from this page — group assignment is a separate concern addressed in the upcoming Budget Management spec.

### Create User Form

```
[ Name* ] [ Email (disabled) ] [ Create User ]
```

- **Name** — required text input.
- **Email** — `<input disabled placeholder="Coming soon">`. Wired to `NewUser.email` but not sent. Same stub as Keys page.

On success: HTMX prepends new row. Form resets.

### User Table Columns

| Column | Source |
|---|---|
| ID | `users.id` |
| Name | `users.name` |
| Email | `users.email` or — |
| Status | Enabled / Disabled tag |
| Created | `users.created_at` |
| Actions | Disable or Enable button |

### Disable User (`POST /admin/users/:id/disable`)

1. `UserRepository::set_enabled(user_id, false)`
2. `ApiKeyRepository::disable_all_keys_for_user(user_id)`
3. Returns updated row fragment

### Enable User (`POST /admin/users/:id/enable`)

1. `UserRepository::set_enabled(user_id, true)` only
2. Does NOT re-enable api_keys — admin re-enables keys individually on the Keys page
3. Returns updated row fragment

### Audit Log — Users Page

| Action | `action` string | `target` | `after_json` |
|---|---|---|---|
| Create user | `"user.create"` | `"user:<id>"` | `{"name":…}` |
| Disable user | `"user.disable"` | `"user:<id>"` | `{"enabled":false}` |
| Enable user | `"user.enable"` | `"user:<id>"` | `{"enabled":true}` |

---

## Nav Bar Changes

`templates/admin/base.html` — replace single `Users` link with two links:

```html
<a href="/admin/keys">Keys</a>
<a href="/admin/users">Users</a>
```

---

## Removed Routes

| Route | Reason |
|---|---|
| `POST /admin/users/:id/rotate-key` | Replaced by `POST /admin/keys/:id/rotate` |
| `POST /admin/api/users/:id/rotate-key` | `rotate_user_key` in routes.rs deleted |

---

## Error Handling

| Scenario | Behaviour |
|---|---|
| Create Key with blank user name | 400 — "user name is required" |
| Create Key — concurrent name collision on auto-create | Catch constraint error, fall back to `find_by_name` |
| Disable/rotate unknown key id | 404 — "key not found" |
| Disable/enable unknown user id | 404 — "user not found" |

---

## Future Step: Welcome Email

When `email` is collected on key creation or user creation:
1. Compose email with subject "Your modelrouter API key"
2. Body includes raw key (shown once) and ready-to-paste `.envrc` block:
   ```
   export ANTHROPIC_API_KEY=<key>
   export ANTHROPIC_BASE_URL=http://<host>:8080
   ```
3. Send via SMTP (future `[email]` section in `config.toml`)

**Not implemented in this spec.** The `email` column exists in the DB and `NewUser` model. The handler contains a `// TODO: send welcome email` stub only.

---

## Success Criteria

- [ ] SQLite migration drops `api_key`, `api_key_old`, `api_key_old_expires_at` from `users`; adds `email`
- [ ] Postgres migration file present at `migrations/postgres/012_keys_users_refactor.sql` with equivalent DDL
- [ ] Auth uses only `api_keys` table — `find_by_api_key` fallback removed from `src/api/auth.rs`
- [ ] `UserRow` struct in `src/db/sqlite/users.rs` updated to remove dropped columns and add `email`
- [ ] `routes.rs` `create_user` compiles without `api_key_hash`; `rotate_user_key` deleted
- [ ] `GET /admin/keys` lists all keys with user name, project, label, status, created
- [ ] Create Key auto-creates user if name not found; uses existing user if found (UNIQUE collision handled)
- [ ] Disabled keys sort to bottom of Keys table
- [ ] Rotate Key: old key disabled, new key created with same user/project/label, token shown once
- [ ] Email field present but disabled on both create forms with "Coming soon" placeholder
- [ ] `GET /admin/users` lists users with ID, name, email, status, created (no group_name column)
- [ ] Create User creates user with no key; `group_name` intentionally absent from form
- [ ] Disable User disables user + all their api_keys
- [ ] Enable User enables user only; does not re-enable api_keys
- [ ] Disabled users sort to bottom of Users table
- [ ] All 6 mutating handlers write audit log entries with correct action strings
- [ ] Nav bar shows both Keys and Users links
- [ ] `cargo test` passes
- [ ] `cargo build --features postgres` passes
