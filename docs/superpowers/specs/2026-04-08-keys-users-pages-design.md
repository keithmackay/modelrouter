# modelrouter — Keys & Users Admin Pages

_Written: 2026-04-08_

---

## Overview

Revamp the admin dashboard to properly separate **key management** from **user management**, and remove the legacy single-key-per-user architecture. After this change:

- All API keys live in the `api_keys` table. The legacy `users.api_key` / `users.api_key_old` / `users.api_key_old_expires_at` columns are dropped.
- Users can be created without a key. Keys are always created explicitly and associated with an existing (or auto-created) user.
- The admin dashboard gains a **Keys** page (new `/admin/keys`) and a repurposed **Users** page (`/admin/users`), linked from the nav bar.

---

## Schema Changes (one migration)

**New migration: `012_keys_users_refactor.sql`**

```sql
-- Drop legacy key columns from users
ALTER TABLE users DROP COLUMN api_key;
ALTER TABLE users DROP COLUMN api_key_old;
ALTER TABLE users DROP COLUMN api_key_old_expires_at;

-- Add email field for future welcome-email feature
ALTER TABLE users ADD COLUMN email TEXT;
```

The `api_keys` table is unchanged. The `group_name` column on `users` is unchanged (used by budget/policy rules).

---

## Model & Repository Changes

### `src/db/models.rs`

`User` struct: remove `api_key`, `api_key_old`, `api_key_old_expires_at` fields. Add `email: Option<String>`.

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

- `list_all_api_keys() -> anyhow::Result<Vec<ApiKey>>` — for the Keys page
- `set_key_enabled(id: i64, enabled: bool) -> anyhow::Result<()>` — for disable (revoke_api_key sets enabled=0 permanently; this allows re-enable)
- `disable_all_keys_for_user(user_id: i64) -> anyhow::Result<()>` — for "disable user" action

Implement all three for both `SqliteDb` and `PostgresDb`.

### `src/db/sqlite/users.rs` and `src/db/postgres/users.rs`

Update all SELECT queries to remove the dropped columns. Remove implementations of `find_by_api_key`, `rotate_key`, `expire_old_keys`.

### `src/api/auth.rs`

Remove the legacy fallback path (step 2: `find_by_api_key`). Auth becomes a single path through `api_keys`.

---

## Keys Page (`/admin/keys`)

### Route

`GET /admin/keys` — renders `keys.html`
`POST /admin/keys` — creates a key (and user if needed)
`POST /admin/keys/:id/disable` — disables a key
`POST /admin/keys/:id/rotate` — revokes key, creates replacement with same user/project/label

### Data

The page loads all rows from `api_keys` joined with `users.name`. It also loads:
- All distinct `users.name` values → for the User datalist
- All distinct non-null `api_keys.project` values → for the Project datalist

Rows are sorted: enabled keys first (by created_at desc), disabled keys after (by created_at desc).

### Create Key Form

```
[ User (datalist)* ] [ Project (datalist) ] [ Label ] [ Email (disabled) ] [ Create Key ]
```

- **User** — required. `<input list="user-list">` + `<datalist id="user-list">` of existing user names. If the submitted name does not match any existing user, a new `users` row is created first (name only, no group, no email).
- **Project** — optional. `<input list="project-list">` + `<datalist id="project-list">` of existing distinct project values.
- **Label** — optional free text (e.g. "laptop", "CI", "claude-code").
- **Email** — `<input disabled placeholder="Coming soon — will email key + .envrc instructions to user">`. Field exists in the DOM; wire up the value into `NewUser.email` but do not send email. Log a `// TODO: send welcome email` stub.

On success: HTMX prepends the new row to the table. The raw key is shown once in the Actions cell in green. Form resets.

### Key Table Columns

| Column | Source |
|---|---|
| User | `users.name` |
| Project | `api_keys.project` or — |
| Label | `api_keys.label` or — |
| Status | Enabled / Disabled tag |
| Created | `api_keys.created_at` |
| Actions | Disable / Rotate buttons |

### Disable Action

`POST /admin/keys/:id/disable` — calls `set_key_enabled(id, false)`. Returns updated row fragment (HTMX outerHTML swap). No page reload; the row stays in place (re-sort happens on next page load).

### Rotate Action

`POST /admin/keys/:id/rotate` — within a single handler:
1. Fetch the existing key row (get user_id, project, label)
2. Call `set_key_enabled(id, false)` on the old key
3. Generate new token, call `create_api_key` with same user_id/project/label
4. Return a new row fragment showing the new key's token once in green

---

## Users Page (`/admin/users`)

### Route

`GET /admin/users` — renders `users.html`
`POST /admin/users` — creates a user (no key)
`POST /admin/users/:id/disable` — disables user + all their keys
`POST /admin/users/:id/enable` — enables user only (does not re-enable keys)

### Data

All rows from `users`. Sorted: enabled users first (by created_at desc), disabled after.

### Create User Form

```
[ Name* ] [ Email (disabled) ] [ Create User ]
```

- **Name** — required text input.
- **Email** — `<input disabled placeholder="Coming soon">`. Wired to `NewUser.email` but not sent.

On success: HTMX prepends new row to table. Form resets.

### User Table Columns

| Column | Source |
|---|---|
| ID | `users.id` |
| Name | `users.name` |
| Email | `users.email` or — |
| Status | Enabled / Disabled tag |
| Created | `users.created_at` |
| Actions | Disable / Enable button |

### Disable Action

`POST /admin/users/:id/disable`:
1. `set_enabled(user_id, false)` on users
2. `disable_all_keys_for_user(user_id)` on api_keys
3. Returns updated row fragment

### Enable Action

`POST /admin/users/:id/enable`:
1. `set_enabled(user_id, true)` on users only
2. Does NOT re-enable keys (admin re-enables keys individually on the Keys page)
3. Returns updated row fragment

---

## Nav Bar Changes

`templates/admin/base.html` — update nav links:

```html
<a href="/admin/keys">Keys</a>
<a href="/admin/users">Users</a>
```

Replace the existing single `<a href="/admin/users">Users</a>` link.

---

## Removed Dashboard Handlers

The following handlers in `src/api/admin/dashboard.rs` are deleted or replaced:

| Handler | Fate |
|---|---|
| `post_create_user` (current) | Replaced — new create-user handler creates user only (no key) |
| `post_rotate_user_key` | Deleted — replaced by `post_rotate_key` on keys page |
| `post_disable_user` | Repurposed — now also disables all keys |
| `post_enable_user` | Repurposed — enables user only |

---

## Removed Routes

```
POST /admin/users/:id/rotate-key   → deleted (use POST /admin/keys/:id/rotate)
```

---

## Error Handling

| Scenario | Behaviour |
|---|---|
| Create Key with blank user name | 400 Bad Request — "user name is required" |
| Create Key — user auto-created but name already exists (race) | Use existing user (find_by_name fallback) |
| Disable/rotate unknown key id | 404 — "key not found" |
| Disable/enable unknown user id | 404 — "user not found" |

---

## Future Step: Welcome Email

When `email` is collected on key creation:
1. Compose email with subject "Your modelrouter API key"
2. Body includes:
   - The raw key (shown once)
   - Ready-to-paste `.envrc` block:
     ```
     export ANTHROPIC_API_KEY=<key>
     export ANTHROPIC_BASE_URL=http://<host>:8080
     ```
3. Send via SMTP (config: `[email]` section in `config.toml`)

This is **not implemented** in this spec. The field is stubbed in the DB, model, and handler only.

---

## Success Criteria

- [ ] Migration drops `api_key`, `api_key_old`, `api_key_old_expires_at` from `users`; adds `email`
- [ ] Auth uses only `api_keys` table — legacy fallback removed
- [ ] `GET /admin/keys` lists all keys with user name, project, label, status, created
- [ ] Create Key form auto-creates user if name not found; uses existing user if found
- [ ] Disabled keys sort to bottom of Keys table
- [ ] Rotate Key: old key disabled, new key created with same user/project/label, token shown once
- [ ] Email field present but disabled on both create forms with "Coming soon" placeholder
- [ ] `GET /admin/users` lists users with ID, name, email, status, created
- [ ] Create User creates user with no key
- [ ] Disable User disables user + all their api_keys
- [ ] Enable User enables user only, does not touch api_keys
- [ ] Disabled users sort to bottom of Users table
- [ ] Nav bar shows both Keys and Users links
- [ ] `cargo test` passes
- [ ] `cargo build --features postgres` passes
