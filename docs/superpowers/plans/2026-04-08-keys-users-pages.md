# Keys & Users Admin Pages Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Revamp the admin dashboard to separate key management from user management, drop legacy `users.api_key*` columns, and add a new `/admin/keys` page alongside a repurposed `/admin/users` page.

**Architecture:** Two SQL migrations drop three legacy columns and add `email`. Repository traits and impls are updated for the new surface area. All dashboard handlers and templates are rewritten or added to support the new pages. No external dependencies added.

**Tech Stack:** Rust, Axum, SQLx (SQLite + Postgres), minijinja templates (compiled via `include_str!`), HTMX

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `migrations/012_keys_users_refactor.sql` | Create | SQLite: drop 3 columns, add email |
| `migrations/postgres/012_keys_users_refactor.sql` | Create | Postgres: same DDL with IF EXISTS guards |
| `src/db/models.rs` | Modify | Remove `api_key*` fields from `User`/`NewUser`; add `email` |
| `src/db/repositories/users.rs` | Modify | Remove 3 trait methods |
| `src/db/sqlite/users.rs` | Modify | Update `UserRow`, `From` impl, all queries; remove 3 impls |
| `src/db/postgres/users.rs` | Modify | Same as SQLite counterpart |
| `src/db/repositories/api_keys.rs` | Modify | Add 3 new trait methods |
| `src/db/sqlite/api_keys.rs` | Modify | Implement 3 new trait methods |
| `src/db/postgres/api_keys.rs` | Modify | Implement 3 new trait methods |
| `src/api/auth.rs` | Modify | Remove legacy `find_by_api_key` fallback |
| `src/api/admin/routes.rs` | Modify | Fix `create_user`; delete `rotate_user_key` |
| `src/cli/mod.rs` | Modify | Fix rotate-key CLI command (uses `UserRepository::rotate_key`) |
| `src/api/admin/dashboard.rs` | Modify | Add Keys page handlers; update Users page handlers; delete `post_rotate_user_key` |
| `templates/admin/keys.html` | Create | Keys page template |
| `templates/admin/users.html` | Modify | Rewrite for user-only management |
| `templates/admin/base.html` | Modify | Add Keys nav link |
| `src/api/admin/templates.rs` | Modify | Register `keys.html` |
| `src/api/app.rs` | Modify | Add Keys routes; remove rotate-key routes |

---

## Task 1: Schema Migrations

**Files:**
- Create: `migrations/012_keys_users_refactor.sql`
- Create: `migrations/postgres/012_keys_users_refactor.sql`

- [ ] **Step 1: Write SQLite migration**

```sql
-- migrations/012_keys_users_refactor.sql
ALTER TABLE users DROP COLUMN api_key;
ALTER TABLE users DROP COLUMN api_key_old;
ALTER TABLE users DROP COLUMN api_key_old_expires_at;
ALTER TABLE users ADD COLUMN email TEXT;
```

- [ ] **Step 2: Write Postgres migration**

```sql
-- migrations/postgres/012_keys_users_refactor.sql
ALTER TABLE users DROP COLUMN IF EXISTS api_key;
ALTER TABLE users DROP COLUMN IF EXISTS api_key_old;
ALTER TABLE users DROP COLUMN IF EXISTS api_key_old_expires_at;
ALTER TABLE users ADD COLUMN IF NOT EXISTS email TEXT;
```

- [ ] **Step 3: Verify migration runs**

```bash
cargo run -- migrate
```
Expected: prints "Applying migration 012" and exits cleanly.

- [ ] **Step 4: Commit**

```bash
git add migrations/012_keys_users_refactor.sql migrations/postgres/012_keys_users_refactor.sql
git commit -m "feat: migration 012 — drop legacy api_key columns, add users.email"
```

---

## Task 2: Update `src/db/models.rs`

**Files:**
- Modify: `src/db/models.rs`

- [ ] **Step 1: Update `User` struct**

Remove fields `api_key`, `api_key_old`, `api_key_old_expires_at`. Add `email: Option<String>`.

Before (relevant section):
```rust
pub struct User {
    pub id: i64,
    pub name: String,
    pub group_name: Option<String>,
    pub api_key: String,
    pub api_key_old: Option<String>,
    pub api_key_old_expires_at: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub spend_usd: f64,
    pub spend_reset_at: Option<String>,
}
```

After:
```rust
pub struct User {
    pub id: i64,
    pub name: String,
    pub group_name: Option<String>,
    pub email: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub spend_usd: f64,
    pub spend_reset_at: Option<String>,
}
```

- [ ] **Step 2: Update `NewUser` struct**

Remove `api_key_hash`. Add `email: Option<String>`.

After:
```rust
pub struct NewUser {
    pub name: String,
    pub group_name: Option<String>,
    pub email: Option<String>,
}
```

- [ ] **Step 3: Build to surface all compile errors (do NOT fix yet — just list them)**

```bash
cargo build 2>&1 | grep "^error" | head -40
```

Expected: errors in `sqlite/users.rs`, `postgres/users.rs`, `api/admin/routes.rs`, `cli/mod.rs`, `api/auth.rs`, and possibly others. These are your task list for subsequent tasks.

- [ ] **Step 4: Commit**

```bash
git add src/db/models.rs
git commit -m "refactor: remove legacy api_key fields from User/NewUser, add email"
```

---

## Task 3: Update `UserRepository` Trait

**Files:**
- Modify: `src/db/repositories/users.rs`

- [ ] **Step 1: Remove 3 trait methods**

Delete the following method signatures from the `UserRepository` trait:
- `async fn find_by_api_key(&self, api_key_hash: &str) -> anyhow::Result<Option<User>>;`
- `async fn rotate_key(&self, user_id: i64, new_hash: &str) -> anyhow::Result<()>;`
- `async fn expire_old_keys(&self) -> anyhow::Result<()>;`

- [ ] **Step 2: Verify trait compiles**

```bash
cargo build -p modelrouter 2>&1 | grep "repositories/users"
```

Expected: no errors from this file.

- [ ] **Step 3: Commit**

```bash
git add src/db/repositories/users.rs
git commit -m "refactor: remove find_by_api_key/rotate_key/expire_old_keys from UserRepository trait"
```

---

## Task 4: Update SQLite `users.rs` Impl

**Files:**
- Modify: `src/db/sqlite/users.rs`

- [ ] **Step 1: Update `UserRow` struct**

Remove `api_key: String`, `api_key_old: Option<String>`, `api_key_old_expires_at: Option<String>`. Add `email: Option<String>`.

- [ ] **Step 2: Update `From<UserRow> for User`**

Remove the 3 dropped fields from the mapping. Add `email: row.email`.

- [ ] **Step 3: Update `list()` SELECT query**

Remove `api_key, api_key_old, api_key_old_expires_at` from the SELECT column list. Add `email`.

- [ ] **Step 4: Update `find_by_id()` SELECT query**

Same column list change.

- [ ] **Step 5: Update `find_by_name()` SELECT query**

Same column list change.

- [ ] **Step 6: Update `create()` INSERT**

The INSERT currently inserts `api_key_hash` into `api_key`. Remove that column from both the column list and the VALUES. Add `email` to both (bound to `new_user.email`).

The RETURNING clause (if present) or subsequent SELECT also needs updating — remove dropped columns, add `email`.

- [ ] **Step 7: Delete `find_by_api_key`, `rotate_key`, `expire_old_keys` impls**

These three `impl UserRepository for SqliteDb` methods no longer exist on the trait — delete them.

- [ ] **Step 8: Build and confirm no errors from this file**

```bash
cargo build 2>&1 | grep "sqlite/users"
```

- [ ] **Step 9: Commit**

```bash
git add src/db/sqlite/users.rs
git commit -m "refactor: update SQLite UserRow and queries for schema 012"
```

---

## Task 5: Update Postgres `users.rs` Impl

**Files:**
- Modify: `src/db/postgres/users.rs`

- [ ] **Step 1: Apply the same changes as Task 4**

Follow the same steps 1–7 as Task 4, with these Postgres-specific notes:
- `UserRow.enabled` is `bool` (not `i64`) — keep as-is
- The `create()` method uses `$1, $2, ...` positional placeholders — recount them after removing `api_key_hash` and adding `email`
- The `RETURNING` clause on the INSERT must also have the columns updated

- [ ] **Step 2: Build and confirm**

```bash
cargo build --features postgres 2>&1 | grep "postgres/users"
```

- [ ] **Step 3: Commit**

```bash
git add src/db/postgres/users.rs
git commit -m "refactor: update Postgres UserRow and queries for schema 012"
```

---

## Task 6: Add 3 Methods to `ApiKeyRepository` Trait

**Files:**
- Modify: `src/db/repositories/api_keys.rs`

- [ ] **Step 1: Add 3 new async method signatures to the trait**

```rust
async fn list_all_api_keys(&self) -> anyhow::Result<Vec<ApiKey>>;
async fn set_key_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()>;
async fn disable_all_keys_for_user(&self, user_id: i64) -> anyhow::Result<()>;
```

Keep `revoke_api_key(id)` — it is preserved for backward compatibility with `POST /admin/api/keys/:id/revoke`.

- [ ] **Step 2: Commit**

```bash
git add src/db/repositories/api_keys.rs
git commit -m "feat: add list_all_api_keys/set_key_enabled/disable_all_keys_for_user to ApiKeyRepository trait"
```

---

## Task 7: Implement New `ApiKeyRepository` Methods — SQLite

**Files:**
- Modify: `src/db/sqlite/api_keys.rs`

- [ ] **Step 1: Implement `list_all_api_keys`**

JOIN `api_keys` with `users` to get `users.name`. Return `Vec<ApiKey>` sorted: enabled first (by `created_at DESC`), disabled after (by `created_at DESC`).

```rust
async fn list_all_api_keys(&self) -> anyhow::Result<Vec<ApiKey>> {
    let rows = sqlx::query_as!(
        ApiKeyRow,
        r#"SELECT k.id, k.user_id, k.key_hash, k.label, k.enabled as "enabled: bool",
                  k.created_at, k.expires_at, k.project
           FROM api_keys k
           ORDER BY k.enabled DESC, k.created_at DESC"#
    )
    .fetch_all(&self.pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}
```

Note: The handler that calls this will JOIN with user names separately or the query can be extended. See Task 10 for how the handler builds the view model (it fetches users separately and does an in-memory join by `user_id`).

- [ ] **Step 2: Implement `set_key_enabled`**

```rust
async fn set_key_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE api_keys SET enabled = ? WHERE id = ?",
        enabled,
        id
    )
    .execute(&self.pool)
    .await?;
    Ok(())
}
```

- [ ] **Step 3: Implement `disable_all_keys_for_user`**

```rust
async fn disable_all_keys_for_user(&self, user_id: i64) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE api_keys SET enabled = FALSE WHERE user_id = ?",
        user_id
    )
    .execute(&self.pool)
    .await?;
    Ok(())
}
```

- [ ] **Step 4: Build and confirm**

```bash
cargo build 2>&1 | grep "sqlite/api_keys"
```

- [ ] **Step 5: Commit**

```bash
git add src/db/sqlite/api_keys.rs
git commit -m "feat: implement list_all_api_keys/set_key_enabled/disable_all_keys_for_user for SQLite"
```

---

## Task 8: Implement New `ApiKeyRepository` Methods — Postgres

**Files:**
- Modify: `src/db/postgres/api_keys.rs`

- [ ] **Step 1: Implement all 3 methods**

Same logic as Task 7, using `$1`/`$2` positional placeholders and `query!` / `query_as!` macros appropriate for Postgres.

`set_key_enabled` Postgres form:
```rust
sqlx::query!(
    "UPDATE api_keys SET enabled = $1 WHERE id = $2",
    enabled,
    id
)
```

`disable_all_keys_for_user` Postgres form:
```rust
sqlx::query!(
    "UPDATE api_keys SET enabled = FALSE WHERE user_id = $1",
    user_id
)
```

- [ ] **Step 2: Build and confirm**

```bash
cargo build --features postgres 2>&1 | grep "postgres/api_keys"
```

- [ ] **Step 3: Commit**

```bash
git add src/db/postgres/api_keys.rs
git commit -m "feat: implement list_all_api_keys/set_key_enabled/disable_all_keys_for_user for Postgres"
```

---

## Task 9: Remove Legacy Auth Fallback

**Files:**
- Modify: `src/api/auth.rs`

- [ ] **Step 1: Read the current auth flow**

Open `src/api/auth.rs` and locate the two-step auth block. It looks like:

```rust
// Step 1: try api_keys table
if let Some(key) = state.db.find_api_key_by_hash(&hash).await? { ... }

// Step 2: legacy fallback — find user by api_key column
if let Some(user) = state.db.find_by_api_key(&hash).await? { ... }
```

- [ ] **Step 2: Delete the fallback block**

Remove step 2 entirely. Auth is now a single path through `api_keys`. The `find_by_api_key` method no longer exists on the trait.

- [ ] **Step 3: Build and confirm**

```bash
cargo build 2>&1 | grep "api/auth"
```

- [ ] **Step 4: Commit**

```bash
git add src/api/auth.rs
git commit -m "refactor: remove legacy users.api_key fallback from auth"
```

---

## Task 10: Update `src/api/admin/routes.rs` and `src/cli/mod.rs`

**Files:**
- Modify: `src/api/admin/routes.rs`
- Modify: `src/cli/mod.rs`

- [ ] **Step 1: Update `CreateUserRequest` and `create_user` in `routes.rs`**

Find `CreateUserRequest`. Remove any key-generation fields (`api_key`, `api_key_hash`).

Find the `create_user` handler. It currently:
1. Generates a UUID key
2. Hashes it
3. Calls `db.create(NewUser { name, api_key_hash: hash, group_name })`

Change it to:
```rust
let user = state.db.create(NewUser { name: req.name.clone(), group_name: req.group_name.clone(), email: None }).await?;
```

No key is generated here anymore.

- [ ] **Step 2: Delete `rotate_user_key` handler from `routes.rs`**

Find `pub async fn rotate_user_key(...)` and delete the entire function.

- [ ] **Step 3: Fix `src/cli/mod.rs` rotate-key command**

Around line 436, `rotate_key` CLI command calls `UserRepository::rotate_key(...)`. Since that method is deleted, replace the implementation:

```rust
// Generate new key
let new_key = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace("-", ""));
let new_hash = sha256_hex(&new_key);
// Disable old keys for user
state.db.disable_all_keys_for_user(user.id).await?;
// Create new key
let api_key = state.db.create_api_key(NewApiKey {
    user_id: user.id,
    key_hash: new_hash,
    label: Some("cli-rotate".to_string()),
    expires_at: None,
    project: None,
}).await?;
println!("New key for {}: {}", user.name, new_key);
```

Import `ApiKeyRepository` and `NewApiKey` at the top of the file if not already present.

- [ ] **Step 4: Build and confirm no remaining errors**

```bash
cargo build 2>&1 | grep -E "routes\.rs|cli/mod"
```

- [ ] **Step 5: Commit**

```bash
git add src/api/admin/routes.rs src/cli/mod.rs
git commit -m "refactor: update create_user and rotate-key CLI to not use UserRepository key methods"
```

---

## Task 11: Create Keys Page Handlers in `dashboard.rs`

**Files:**
- Modify: `src/api/admin/dashboard.rs`

This is the largest task. Work section by section.

### View model struct

- [ ] **Step 1: Add `KeyView` struct**

Add a view model that bundles an `ApiKey` with the user's name (for template rendering):

```rust
#[derive(serde::Serialize)]
struct KeyView {
    id: i64,
    user_id: i64,
    user_name: String,
    project: Option<String>,
    label: Option<String>,
    enabled: bool,
    created_at: String,
    raw_key: Option<String>, // shown once after create/rotate
}
```

### `get_keys` handler

- [ ] **Step 2: Implement `get_keys`**

```rust
pub async fn get_keys(
    State(state): State<AppState>,
    session: DashboardSession,
) -> Result<impl IntoResponse, DashboardError> {
    let keys = state.db.list_all_api_keys().await?;
    let users = state.db.list().await?; // UserRepository::list
    let user_map: std::collections::HashMap<i64, String> = users
        .iter()
        .map(|u| (u.id, u.name.clone()))
        .collect();

    let key_views: Vec<KeyView> = keys.into_iter().map(|k| {
        let user_name = user_map.get(&k.user_id).cloned().unwrap_or_default();
        KeyView {
            id: k.id,
            user_id: k.user_id,
            user_name,
            project: k.project,
            label: k.label,
            enabled: k.enabled,
            created_at: k.created_at,
            raw_key: None,
        }
    }).collect();

    // All existing user names for datalist
    let user_names: Vec<String> = users.iter().map(|u| u.name.clone()).collect();
    // All distinct project values for datalist
    let mut projects: Vec<String> = key_views.iter()
        .filter_map(|k| k.project.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    projects.sort();

    let tmpl = state.templates.get_template("keys.html")?;
    let ctx = minijinja::context! {
        session => session.claims(),
        keys => key_views,
        user_names => user_names,
        projects => projects,
    };
    Ok(Html(tmpl.render(ctx)?))
}
```

### `post_create_key` handler

- [ ] **Step 3: Implement `post_create_key`**

```rust
pub async fn post_create_key(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Form(form): Form<CreateKeyForm>,
) -> Result<impl IntoResponse, DashboardError> {
    if form.user_name.trim().is_empty() {
        return Err(DashboardError::BadRequest("user name is required".into()));
    }

    // Find or auto-create user
    let user = match state.db.find_by_name(&form.user_name).await? {
        Some(u) => u,
        None => {
            match state.db.create(NewUser {
                name: form.user_name.clone(),
                group_name: None,
                email: None,
            }).await {
                Ok(u) => u,
                Err(_) => {
                    // UNIQUE collision — concurrent create
                    state.db.find_by_name(&form.user_name).await?
                        .ok_or_else(|| DashboardError::Internal("user not found after collision".into()))?
                }
            }
        }
    };

    // Generate key
    let raw_key = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace("-", ""));
    let key_hash = sha256_hex(&raw_key);

    let new_key = state.db.create_api_key(NewApiKey {
        user_id: user.id,
        key_hash,
        label: if form.label.trim().is_empty() { None } else { Some(form.label.clone()) },
        expires_at: None,
        project: if form.project.trim().is_empty() { None } else { Some(form.project.clone()) },
    }).await?;

    // TODO: send welcome email if form.email is set

    audit(&state.db, &session, "key.create", &format!("key:{}", new_key.id),
        serde_json::json!({ "user_id": user.id, "project": new_key.project, "label": new_key.label })
    ).await?;

    let view = KeyView {
        id: new_key.id,
        user_id: user.id,
        user_name: user.name,
        project: new_key.project,
        label: new_key.label,
        enabled: true,
        created_at: new_key.created_at,
        raw_key: Some(raw_key),
    };

    Ok(Html(key_row_html(&view)))
}
```

Add the form struct:
```rust
#[derive(serde::Deserialize)]
pub struct CreateKeyForm {
    pub user_name: String,
    pub project: String,
    pub label: String,
    pub email: String, // collected but not acted on yet
}
```

### `post_disable_key` handler

- [ ] **Step 4: Implement `post_disable_key`**

```rust
pub async fn post_disable_key(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, DashboardError> {
    // Verify key exists
    let keys = state.db.list_all_api_keys().await?;
    let key = keys.into_iter().find(|k| k.id == id)
        .ok_or_else(|| DashboardError::NotFound("key not found".into()))?;

    state.db.set_key_enabled(id, false).await?;

    audit(&state.db, &session, "key.disable", &format!("key:{}", id),
        serde_json::json!({ "enabled": false })
    ).await?;

    let users = state.db.list().await?;
    let user_name = users.iter().find(|u| u.id == key.user_id)
        .map(|u| u.name.clone()).unwrap_or_default();

    let view = KeyView {
        id: key.id,
        user_id: key.user_id,
        user_name,
        project: key.project,
        label: key.label,
        enabled: false,
        created_at: key.created_at,
        raw_key: None,
    };

    Ok(Html(key_row_html(&view)))
}
```

### `post_rotate_key` handler

- [ ] **Step 5: Implement `post_rotate_key`**

```rust
pub async fn post_rotate_key(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, DashboardError> {
    let keys = state.db.list_all_api_keys().await?;
    let old_key = keys.into_iter().find(|k| k.id == id)
        .ok_or_else(|| DashboardError::NotFound("key not found".into()))?;

    state.db.set_key_enabled(id, false).await?;

    let raw_key = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace("-", ""));
    let key_hash = sha256_hex(&raw_key);

    let new_key = state.db.create_api_key(NewApiKey {
        user_id: old_key.user_id,
        key_hash,
        label: old_key.label.clone(),
        expires_at: None,
        project: old_key.project.clone(),
    }).await?;

    audit(&state.db, &session, "key.rotate", &format!("key:{}", new_key.id),
        serde_json::json!({ "user_id": old_key.user_id, "replaced_key_id": id })
    ).await?;

    let users = state.db.list().await?;
    let user_name = users.iter().find(|u| u.id == old_key.user_id)
        .map(|u| u.name.clone()).unwrap_or_default();

    let view = KeyView {
        id: new_key.id,
        user_id: old_key.user_id,
        user_name,
        project: new_key.project,
        label: new_key.label,
        enabled: true,
        created_at: new_key.created_at,
        raw_key: Some(raw_key),
    };

    Ok(Html(key_row_html(&view)))
}
```

### `key_row_html` helper

- [ ] **Step 6: Implement `key_row_html` helper**

```rust
fn key_row_html(view: &KeyView) -> String {
    let status_tag = if view.enabled {
        r#"<span class="tag tag-enabled">Enabled</span>"#
    } else {
        r#"<span class="tag tag-disabled">Disabled</span>"#
    };

    let raw_key_html = if let Some(ref raw) = view.raw_key {
        format!(r#"<span style="color:green;font-family:monospace;font-size:0.85rem;">{}</span>"#, raw)
    } else {
        String::new()
    };

    let disable_btn = if view.enabled {
        format!(
            r#"<button class="btn btn-danger" hx-post="/admin/keys/{}/disable" hx-target="#key-row-{}" hx-swap="outerHTML">Disable</button>"#,
            view.id, view.id
        )
    } else {
        String::new()
    };

    format!(
        r#"<tr id="key-row-{}">
          <td>{}</td>
          <td>{}</td>
          <td>{}</td>
          <td>{}</td>
          <td>{}</td>
          <td>
            {}
            <button class="btn btn-secondary" hx-post="/admin/keys/{}/rotate" hx-target="#key-row-{}" hx-swap="outerHTML">Rotate</button>
            {}
          </td>
        </tr>"#,
        view.id,
        view.user_name,
        view.project.as_deref().unwrap_or("—"),
        view.label.as_deref().unwrap_or("—"),
        status_tag,
        view.created_at,
        disable_btn,
        view.id, view.id,
        raw_key_html
    )
}
```

- [ ] **Step 7: Build keys page handlers**

```bash
cargo build 2>&1 | grep "dashboard"
```

Fix any compile errors before proceeding.

- [ ] **Step 8: Commit keys page handlers**

```bash
git add src/api/admin/dashboard.rs
git commit -m "feat: add Keys page handlers (get_keys, post_create_key, post_disable_key, post_rotate_key)"
```

---

## Task 12: Update Users Page Handlers in `dashboard.rs`

**Files:**
- Modify: `src/api/admin/dashboard.rs`

- [ ] **Step 1: Delete `post_rotate_user_key` handler**

Find and delete the entire `post_rotate_user_key` function. It calls `UserRepository::rotate_key` which no longer exists.

- [ ] **Step 2: Update `post_disable_user` to also disable all keys**

After calling `set_enabled(id, false)`, add:
```rust
state.db.disable_all_keys_for_user(id).await?;
```

Write audit log with action `"user.disable"`, target `"user:<id>"`, after_json `{"enabled": false}`.

- [ ] **Step 3: Ensure `post_enable_user` does NOT re-enable keys**

`post_enable_user` should only call `set_enabled(id, true)`. Confirm it does not touch `api_keys`.

Write audit log with action `"user.enable"`, target `"user:<id>"`, after_json `{"enabled": true}`.

- [ ] **Step 4: Update `post_create_user` to use new `NewUser`**

```rust
let user = state.db.create(NewUser {
    name: form.name.clone(),
    group_name: None, // group_name intentionally omitted from this form
    email: None,
}).await?;
```

Write audit log with action `"user.create"`, target `"user:<id>"`, after_json `{"name": user.name}`.

- [ ] **Step 5: Update `user_row_html` helper**

Remove the "Rotate Key" button. Change columns to: ID / Name / Email / Status / Created / Actions (Disable or Enable).

```rust
fn user_row_html(user: &User) -> String {
    let status_tag = if user.enabled {
        r#"<span class="tag tag-enabled">Enabled</span>"#
    } else {
        r#"<span class="tag tag-disabled">Disabled</span>"#
    };

    let action_btn = if user.enabled {
        format!(
            r#"<button class="btn btn-danger" hx-post="/admin/users/{}/disable" hx-target="#user-row-{}" hx-swap="outerHTML">Disable</button>"#,
            user.id, user.id
        )
    } else {
        format!(
            r#"<button class="btn btn-success" hx-post="/admin/users/{}/enable" hx-target="#user-row-{}" hx-swap="outerHTML">Enable</button>"#,
            user.id, user.id
        )
    };

    format!(
        r#"<tr id="user-row-{}">
          <td>{}</td>
          <td>{}</td>
          <td>{}</td>
          {}
          <td>{}</td>
          <td>{}</td>
        </tr>"#,
        user.id,
        user.id,
        user.name,
        user.email.as_deref().unwrap_or("—"),
        status_tag,
        user.created_at,
        action_btn
    )
}
```

- [ ] **Step 6: Build and confirm**

```bash
cargo build 2>&1 | grep "dashboard"
```

- [ ] **Step 7: Commit**

```bash
git add src/api/admin/dashboard.rs
git commit -m "feat: update Users page handlers — disable also disables keys, remove rotate, add email column"
```

---

## Task 13: Create `templates/admin/keys.html`

**Files:**
- Create: `templates/admin/keys.html`

- [ ] **Step 1: Write the keys page template**

```html
{% extends "base.html" %}
{% block title %}Keys — modelrouter Admin{% endblock %}
{% block content %}
<h1>Keys</h1>

<datalist id="user-list">
    {% for name in user_names %}<option value="{{ name }}">{% endfor %}
</datalist>
<datalist id="project-list">
    {% for p in projects %}<option value="{{ p }}">{% endfor %}
</datalist>

<form hx-post="/admin/keys" hx-target="#keys-tbody" hx-swap="afterbegin" hx-on::after-request="this.reset()" style="display:flex;gap:0.5rem;align-items:flex-end;margin-bottom:1.5rem;">
    <div>
        <label style="display:block;font-size:0.85rem;margin-bottom:0.25rem;">User *</label>
        <input type="text" name="user_name" required list="user-list" placeholder="alice" style="padding:0.4rem 0.6rem;border:1px solid #ccc;border-radius:4px;">
    </div>
    <div>
        <label style="display:block;font-size:0.85rem;margin-bottom:0.25rem;">Project</label>
        <input type="text" name="project" list="project-list" placeholder="my-project" style="padding:0.4rem 0.6rem;border:1px solid #ccc;border-radius:4px;">
    </div>
    <div>
        <label style="display:block;font-size:0.85rem;margin-bottom:0.25rem;">Label</label>
        <input type="text" name="label" placeholder="laptop" style="padding:0.4rem 0.6rem;border:1px solid #ccc;border-radius:4px;">
    </div>
    <div>
        <label style="display:block;font-size:0.85rem;margin-bottom:0.25rem;">Email</label>
        <input type="email" name="email" disabled placeholder="Coming soon — will email key to user" style="padding:0.4rem 0.6rem;border:1px solid #ccc;border-radius:4px;opacity:0.6;">
    </div>
    <button type="submit" class="btn btn-success">Create Key</button>
</form>

<table>
    <thead>
        <tr>
            <th>User</th>
            <th>Project</th>
            <th>Label</th>
            <th>Status</th>
            <th>Created</th>
            <th>Actions</th>
        </tr>
    </thead>
    <tbody id="keys-tbody">
        {% for key in keys %}
        <tr id="key-row-{{ key.id }}">
            <td>{{ key.user_name }}</td>
            <td>{{ key.project | default(value="—") }}</td>
            <td>{{ key.label | default(value="—") }}</td>
            <td>
                {% if key.enabled %}
                <span class="tag tag-enabled">Enabled</span>
                {% else %}
                <span class="tag tag-disabled">Disabled</span>
                {% endif %}
            </td>
            <td>{{ key.created_at }}</td>
            <td>
                {% if key.enabled %}
                <button class="btn btn-danger" hx-post="/admin/keys/{{ key.id }}/disable" hx-target="#key-row-{{ key.id }}" hx-swap="outerHTML">Disable</button>
                {% endif %}
                <button class="btn btn-secondary" hx-post="/admin/keys/{{ key.id }}/rotate" hx-target="#key-row-{{ key.id }}" hx-swap="outerHTML">Rotate</button>
                {% if key.raw_key %}
                <span style="color:green;font-family:monospace;font-size:0.85rem;">{{ key.raw_key }}</span>
                {% endif %}
            </td>
        </tr>
        {% endfor %}
    </tbody>
</table>
{% endblock %}
```

- [ ] **Step 2: Commit**

```bash
git add templates/admin/keys.html
git commit -m "feat: add keys.html admin template"
```

---

## Task 14: Rewrite `templates/admin/users.html`

**Files:**
- Modify: `templates/admin/users.html`

- [ ] **Step 1: Rewrite the template**

```html
{% extends "base.html" %}
{% block title %}Users — modelrouter Admin{% endblock %}
{% block content %}
<h1>Users</h1>

<form hx-post="/admin/users" hx-target="#users-tbody" hx-swap="afterbegin" hx-on::after-request="this.reset()" style="display:flex;gap:0.5rem;align-items:flex-end;margin-bottom:1.5rem;">
    <div>
        <label style="display:block;font-size:0.85rem;margin-bottom:0.25rem;">Name *</label>
        <input type="text" name="name" required placeholder="alice" style="padding:0.4rem 0.6rem;border:1px solid #ccc;border-radius:4px;">
    </div>
    <div>
        <label style="display:block;font-size:0.85rem;margin-bottom:0.25rem;">Email</label>
        <input type="email" name="email" disabled placeholder="Coming soon" style="padding:0.4rem 0.6rem;border:1px solid #ccc;border-radius:4px;opacity:0.6;">
    </div>
    <button type="submit" class="btn btn-success">Create User</button>
</form>

<table>
    <thead>
        <tr>
            <th>ID</th>
            <th>Name</th>
            <th>Email</th>
            <th>Status</th>
            <th>Created</th>
            <th>Actions</th>
        </tr>
    </thead>
    <tbody id="users-tbody">
        {% for user in users %}
        <tr id="user-row-{{ user.id }}">
            <td>{{ user.id }}</td>
            <td>{{ user.name }}</td>
            <td>{{ user.email | default(value="—") }}</td>
            <td>
                {% if user.enabled %}
                <span class="tag tag-enabled">Enabled</span>
                {% else %}
                <span class="tag tag-disabled">Disabled</span>
                {% endif %}
            </td>
            <td>{{ user.created_at }}</td>
            <td>
                {% if user.enabled %}
                <button class="btn btn-danger" hx-post="/admin/users/{{ user.id }}/disable" hx-target="#user-row-{{ user.id }}" hx-swap="outerHTML">Disable</button>
                {% else %}
                <button class="btn btn-success" hx-post="/admin/users/{{ user.id }}/enable" hx-target="#user-row-{{ user.id }}" hx-swap="outerHTML">Enable</button>
                {% endif %}
            </td>
        </tr>
        {% endfor %}
    </tbody>
</table>
{% endblock %}
```

- [ ] **Step 2: Commit**

```bash
git add templates/admin/users.html
git commit -m "feat: rewrite users.html — user-only management, email column, no group/rotate-key"
```

---

## Task 15: Update Nav and Register Template

**Files:**
- Modify: `templates/admin/base.html`
- Modify: `src/api/admin/templates.rs`

- [ ] **Step 1: Add Keys link to nav in `base.html`**

Find the existing Users nav link. Add a Keys link before it:

```html
<a href="/admin/keys">Keys</a>
<a href="/admin/users">Users</a>
```

- [ ] **Step 2: Register `keys.html` in `templates.rs`**

In `src/api/admin/templates.rs`, find where `users.html` is registered (pattern: `env.add_template_owned("users.html", include_str!("../../../templates/admin/users.html").to_string())?;`).

Add the same line for `keys.html`:
```rust
env.add_template_owned("keys.html", include_str!("../../../templates/admin/keys.html").to_string())?;
```

- [ ] **Step 3: Commit**

```bash
git add templates/admin/base.html src/api/admin/templates.rs
git commit -m "feat: add Keys nav link and register keys.html template"
```

---

## Task 16: Wire Up Routes in `app.rs`

**Files:**
- Modify: `src/api/app.rs`

- [ ] **Step 1: Update imports**

In the `use crate::api::admin::dashboard::{ ... }` import block:
- Remove: `post_rotate_user_key`
- Add: `get_keys, post_create_key, post_disable_key, post_rotate_key`

In the `use crate::api::admin::routes::{ ... }` import block:
- Remove: `rotate_user_key`

- [ ] **Step 2: Remove deleted routes**

Delete these lines:
```rust
.route("/admin/users/:id/rotate-key", post(post_rotate_user_key))
.route("/admin/api/users/:id/rotate-key", post(rotate_user_key))
```

- [ ] **Step 3: Add Keys page routes**

After the `/admin/users/:id/enable` route, add:
```rust
.route("/admin/keys", get(get_keys).post(post_create_key))
.route("/admin/keys/:id/disable", post(post_disable_key))
.route("/admin/keys/:id/rotate", post(post_rotate_key))
```

- [ ] **Step 4: Build clean**

```bash
cargo build
```

Expected: no errors.

- [ ] **Step 5: Run tests**

```bash
cargo test
```

Expected: all tests pass.

- [ ] **Step 6: Verify Postgres feature build**

```bash
cargo build --features postgres
```

Expected: no errors.

- [ ] **Step 7: Commit**

```bash
git add src/api/app.rs
git commit -m "feat: wire up /admin/keys routes; remove rotate-key routes"
```

---

## Task 17: Smoke Test

**Files:** None (runtime verification)

- [ ] **Step 1: Run migration and start server**

```bash
cargo run -- migrate
cargo run -- serve
```

- [ ] **Step 2: Log in as superadmin at `http://localhost:8080/admin`**

- [ ] **Step 3: Verify nav shows Keys and Users links**

- [ ] **Step 4: Go to `/admin/keys`**
  - Page loads with Create Key form and keys table
  - Create a key for an existing user — key appears in table, raw token shown in green
  - Create a key for a new user name — user is auto-created, key appears
  - Disable a key — row updates in place, button disappears
  - Rotate a key — old row replaced with new row, new token shown in green

- [ ] **Step 5: Go to `/admin/users`**
  - Page loads: ID / Name / Email / Status / Created / Actions columns
  - No "Rotate Key" button present
  - No "Group" column
  - Create a user — row prepended, no key created
  - Disable a user — all their keys also disabled (verify on Keys page)
  - Enable a user — user re-enabled, keys remain disabled

- [ ] **Step 6: Commit if any last-minute fixes were needed**

```bash
git add -p
git commit -m "fix: smoke test corrections"
```

---

## Final Build Verification

- [ ] `cargo test` passes
- [ ] `cargo build --features postgres` passes
- [ ] `cargo build --features bedrock` passes (no regressions)
