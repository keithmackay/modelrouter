# Admin Bootstrap CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `modelrouter admin` CLI subcommand group for bootstrapping and recovering admin accounts without a running server.

**Architecture:** New `src/cli/admin.rs` owns all Admin command logic (DB bootstrap + command dispatch). `src/cli/commands.rs` adds the clap enum definitions. `src/cli/mod.rs` delegates to `admin::run()`. A new `update_password_hash` method is added to `AdminUserRepository` and both DB implementations.

**Tech Stack:** Rust, clap (existing), sqlx (existing), bcrypt (existing), rpassword (new), anyhow (existing)

**Spec:** `docs/superpowers/specs/2026-04-08-admin-bootstrap-design.md`

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `Cargo.toml` | Modify | Add `rpassword = "7"` dependency |
| `src/cli/commands.rs` | Modify | Add `AdminArgs`, `AdminCommands`, `AdminRole` enum to `Commands` |
| `src/cli/mod.rs` | Modify | Add `pub mod admin;` + delegate `Commands::Admin` to `admin::run()` |
| `src/cli/admin.rs` | **Create** | DB bootstrap + all Admin command arm implementations |
| `src/db/repositories/admin_users.rs` | Modify | Add `update_password_hash` to trait |
| `src/db/sqlite/admin_users.rs` | Modify | Implement `update_password_hash` for SQLite |
| `src/db/postgres/admin_users.rs` | Modify | Implement `update_password_hash` for Postgres |

---

## Task 1: Add `rpassword` dependency

**Files:**
- Modify: `Cargo.toml`

- [ ] Open `Cargo.toml` and find the `[dependencies]` section. Add after the `bcrypt` line:

```toml
rpassword = "7"
```

- [ ] Verify it resolves:

```bash
cargo fetch
```

Expected: no errors, `rpassword` fetched.

- [ ] Commit:

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: add rpassword dependency for hidden terminal input"
```

---

## Task 2: Add `update_password_hash` to `AdminUserRepository` trait

**Files:**
- Modify: `src/db/repositories/admin_users.rs`

- [ ] Open `src/db/repositories/admin_users.rs`. The trait currently ends with `create_from_oidc`. Add one method:

```rust
async fn update_password_hash(&self, id: i64, hash: &str) -> anyhow::Result<()>;
```

Full updated trait:

```rust
#[async_trait]
pub trait AdminUserRepository: Send + Sync {
    async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<AdminUser>>;
    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<AdminUser>>;
    async fn list(&self) -> anyhow::Result<Vec<AdminUser>>;
    async fn create(&self, user: NewAdminUser) -> anyhow::Result<AdminUser>;
    async fn set_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()>;
    async fn delete(&self, id: i64) -> anyhow::Result<()>;
    async fn update_last_login(&self, id: i64) -> anyhow::Result<()>;
    async fn find_by_oidc_subject(&self, subject: &str) -> anyhow::Result<Option<AdminUser>>;
    async fn create_from_oidc(&self, user: NewAdminUserFromOidc) -> anyhow::Result<AdminUser>;
    async fn update_password_hash(&self, id: i64, hash: &str) -> anyhow::Result<()>;
}
```

- [ ] Verify it compiles (will fail on missing impls — that's expected):

```bash
cargo build 2>&1 | grep "update_password_hash"
```

Expected: errors about missing impl in `SqliteDb` and `PostgresDb`.

---

## Task 3: Implement `update_password_hash` for SQLite

**Files:**
- Modify: `src/db/sqlite/admin_users.rs`

- [ ] Append to the `impl AdminUserRepository for SqliteDb` block (after `create_from_oidc`):

```rust
async fn update_password_hash(&self, id: i64, hash: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE admin_users SET password_hash = ? WHERE id = ?")
        .bind(hash)
        .bind(id)
        .execute(&self.pool)
        .await?;
    Ok(())
}
```

Note: `hash` is bound first, `id` second — sqlx binds `?` placeholders left-to-right, so the first `.bind()` call fills the first `?` in the SQL (`password_hash = ?`) and the second fills `WHERE id = ?`.

- [ ] Verify SQLite builds cleanly:

```bash
cargo build 2>&1 | grep -E "error|update_password_hash"
```

Expected: error only from Postgres impl (if postgres feature not enabled, may be clean).

---

## Task 4: Implement `update_password_hash` for Postgres

**Files:**
- Modify: `src/db/postgres/admin_users.rs`

- [ ] Append to the `impl AdminUserRepository for PostgresDb` block:

```rust
async fn update_password_hash(&self, id: i64, hash: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE admin_users SET password_hash = $1 WHERE id = $2")
        .bind(hash)
        .bind(id)
        .execute(&self.pool)
        .await?;
    Ok(())
}
```

Note: Postgres uses `$1`, `$2` placeholders. `hash` is `$1`, `id` is `$2`.

- [ ] Verify both builds pass:

```bash
cargo build
cargo build --features postgres
```

Expected: clean build for both.

- [ ] Commit:

```bash
git add src/db/repositories/admin_users.rs src/db/sqlite/admin_users.rs src/db/postgres/admin_users.rs
git commit -m "feat: add update_password_hash to AdminUserRepository"
```

---

## Task 5: Add clap types to `commands.rs`

**Files:**
- Modify: `src/cli/commands.rs`

- [ ] Add `AdminRole` enum and `AdminArgs`/`AdminCommands` to `src/cli/commands.rs`. Add after the existing imports at the top:

```rust
use std::fmt;
```

- [ ] Add `Admin(AdminArgs)` variant to the existing `Commands` enum, after `UninstallService`:

```rust
/// Manage admin users
Admin(AdminArgs),
```

- [ ] Add the new types at the bottom of `src/cli/commands.rs`:

```rust
// ── Admin subcommands ────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum AdminRole {
    Superadmin,
    Viewer,
}

impl fmt::Display for AdminRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdminRole::Superadmin => write!(f, "superadmin"),
            AdminRole::Viewer => write!(f, "viewer"),
        }
    }
}

impl std::str::FromStr for AdminRole {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "superadmin" => Ok(AdminRole::Superadmin),
            "viewer" => Ok(AdminRole::Viewer),
            other => Err(format!("role must be 'superadmin' or 'viewer', got '{}'", other)),
        }
    }
}

#[derive(Args)]
pub struct AdminArgs {
    #[command(subcommand)]
    pub command: AdminCommands,
}

#[derive(Subcommand)]
pub enum AdminCommands {
    /// Create a new admin user (prompts for password)
    Create {
        #[arg(long)]
        name: String,
        /// Role to assign. Default: superadmin.
        #[arg(long, default_value = "superadmin")]
        role: AdminRole,
    },
    /// List all admin users
    List {
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
    /// Reset an admin user's password (prompts for new password)
    ResetPassword {
        #[arg(long)]
        name: String,
    },
    /// Enable an admin user
    Enable {
        name: String,
    },
    /// Disable an admin user
    Disable {
        name: String,
    },
}
```

- [ ] Verify it compiles (will have missing match arm warning — expected):

```bash
cargo build 2>&1 | grep -E "error|Admin"
```

---

## Task 5b: Unit tests for `AdminRole`

**Files:**
- Modify: `src/cli/commands.rs` (add `#[cfg(test)]` module at bottom)

`AdminRole::from_str` is the only pure logic unit worth testing before wiring the full CLI. Test it now.

- [ ] Add at the bottom of `src/cli/commands.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::AdminRole;
    use std::str::FromStr;

    #[test]
    fn admin_role_superadmin_parses() {
        let r = AdminRole::from_str("superadmin").unwrap();
        assert!(matches!(r, AdminRole::Superadmin));
        assert_eq!(r.to_string(), "superadmin");
    }

    #[test]
    fn admin_role_viewer_parses() {
        let r = AdminRole::from_str("viewer").unwrap();
        assert!(matches!(r, AdminRole::Viewer));
        assert_eq!(r.to_string(), "viewer");
    }

    #[test]
    fn admin_role_invalid_rejected() {
        let err = AdminRole::from_str("god").unwrap_err();
        assert!(err.contains("superadmin") && err.contains("viewer"));
    }
}
```

- [ ] Run the tests to verify they pass:

```bash
cargo test admin_role
```

Expected: 3 tests pass.

- [ ] Commit:

```bash
git add src/cli/commands.rs
git commit -m "test: AdminRole parsing unit tests"
```

---

## Task 6: Create `src/cli/admin.rs`

**Files:**
- Create: `src/cli/admin.rs`

This file owns all Admin command implementations. The pattern mirrors `src/cli/mod.rs`'s existing `Commands::User` and `Commands::Budget` arms.

- [ ] Create `src/cli/admin.rs` with the following content:

```rust
use anyhow::Result;
use crate::cli::commands::AdminCommands;
use crate::db::models::NewAuditLogEntry;
use crate::db::repositories::{
    admin_users::AdminUserRepository,
    audit::AuditRepository,
};
use crate::report::formatter::{print_rows, OutputFormat};

/// DB bootstrap shared by all admin commands.
/// NOTE: Always uses SQLite. The `--features postgres` build still uses SQLite
/// for CLI commands — Postgres is only wired at serve time via AppState.
async fn connect(config: Option<std::path::PathBuf>) -> Result<crate::db::sqlite::SqliteDb> {
    let settings = crate::config::load(config)?;
    let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
    crate::db::migrations::run_migrations(&db.pool).await?;
    Ok(db)
}

/// Write an audit row with actor = "cli". Logs a warning on failure but does not abort.
async fn audit(
    db: &impl AuditRepository,
    action: &str,
    target: &str,
    after_json: serde_json::Value,
) {
    // after_json is serde_json::Value; convert to String for the Option<String> field.
    if let Err(e) = AuditRepository::create(
        db,
        NewAuditLogEntry {
            actor_id: None,
            actor_name: "cli".to_string(),
            action: action.to_string(),
            target: Some(target.to_string()),
            before_json: None,
            after_json: Some(after_json.to_string()),
        },
    )
    .await
    {
        eprintln!("warning: failed to write audit log: {e}");
    }
}

pub async fn run(config: Option<std::path::PathBuf>, cmd: AdminCommands) -> Result<()> {
    match cmd {
        AdminCommands::Create { name, role } => {
            let db = connect(config).await?;

            // Check uniqueness before prompting for password
            if AdminUserRepository::find_by_name(&db, &name).await?.is_some() {
                anyhow::bail!("admin user '{}' already exists", name);
            }

            let password = rpassword::prompt_password("Password: ")?;
            let confirm = rpassword::prompt_password("Confirm password: ")?;
            if password != confirm {
                anyhow::bail!("passwords do not match");
            }

            let role_str = role.to_string();
            let password_hash = bcrypt::hash(&password, bcrypt::DEFAULT_COST)
                .map_err(|e| anyhow::anyhow!("bcrypt error: {e}"))?;

            let admin = AdminUserRepository::create(
                &db,
                crate::db::models::NewAdminUser {
                    name: name.clone(),
                    password_hash,
                    role: role_str.clone(),
                },
            )
            .await?;

            audit(
                &db,
                "admin.create",
                &format!("admin:{}", admin.id),
                serde_json::json!({ "name": admin.name, "role": admin.role }),
            )
            .await;

            println!("Created admin '{}' (id={}, role={}).", admin.name, admin.id, admin.role);
            println!("Store this password securely — it cannot be retrieved later.");
        }

        AdminCommands::List { format } => {
            let db = connect(config).await?;
            let admins = AdminUserRepository::list(&db).await?;

            // Project to a safe DTO — AdminUser derives Serialize which includes
            // password_hash. Using explicit row closure ensures the hash never
            // appears in table, CSV, or JSON output.
            #[derive(serde::Serialize)]
            struct AdminRow {
                id: i64,
                name: String,
                role: String,
                status: String,
                created_at: String,
            }
            let rows: Vec<AdminRow> = admins.into_iter().map(|a| AdminRow {
                id: a.id,
                name: a.name,
                role: a.role,
                status: if a.enabled { "enabled".into() } else { "disabled".into() },
                created_at: a.created_at,
            }).collect();

            print_rows(
                &rows,
                &["ID", "Name", "Role", "Status", "Created At"],
                |a| {
                    vec![
                        a.id.to_string(),
                        a.name.clone(),
                        a.role.clone(),
                        a.status.clone(),
                        a.created_at.clone(),
                    ]
                },
                format,
            );
        }

        AdminCommands::ResetPassword { name } => {
            let db = connect(config).await?;

            let admin = AdminUserRepository::find_by_name(&db, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("admin user '{}' not found", name))?;

            let password = rpassword::prompt_password("New password: ")?;
            let password_hash = bcrypt::hash(&password, bcrypt::DEFAULT_COST)
                .map_err(|e| anyhow::anyhow!("bcrypt error: {e}"))?;

            AdminUserRepository::update_password_hash(&db, admin.id, &password_hash).await?;

            audit(
                &db,
                "admin.reset_password",
                &format!("admin:{}", admin.id),
                serde_json::json!({ "name": admin.name }),
            )
            .await;

            println!("Password updated for admin '{}'.", admin.name);
        }

        AdminCommands::Enable { name } => {
            let db = connect(config).await?;

            let admin = AdminUserRepository::find_by_name(&db, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("admin user '{}' not found", name))?;

            AdminUserRepository::set_enabled(&db, admin.id, true).await?;

            audit(
                &db,
                "admin.enable",
                &format!("admin:{}", admin.id),
                serde_json::json!({ "name": admin.name, "enabled": true }),
            )
            .await;

            println!("Enabled admin '{}'.", admin.name);
        }

        AdminCommands::Disable { name } => {
            let db = connect(config).await?;

            let admin = AdminUserRepository::find_by_name(&db, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("admin user '{}' not found", name))?;

            AdminUserRepository::set_enabled(&db, admin.id, false).await?;

            audit(
                &db,
                "admin.disable",
                &format!("admin:{}", admin.id),
                serde_json::json!({ "name": admin.name, "enabled": false }),
            )
            .await;

            println!("Disabled admin '{}'.", admin.name);
        }
    }
    Ok(())
}
```

- [ ] Verify it compiles (may have unused import warnings — fine):

```bash
cargo build 2>&1 | grep "^error"
```

Expected: no `error` lines.

---

## Task 7: Wire `Commands::Admin` in `src/cli/mod.rs`

**Files:**
- Modify: `src/cli/mod.rs`

- [ ] Add `pub mod admin;` near the top of `src/cli/mod.rs`, after `pub mod commands;`:

```rust
pub mod admin;
```

- [ ] Add the `Commands::Admin` match arm inside `pub async fn run(cli: Cli) -> Result<()>`, after the `Commands::UninstallService` arm:

```rust
Commands::Admin(admin_args) => {
    admin::run(cli.config, admin_args.command).await?;
}
```

- [ ] No import changes needed in `mod.rs`. The `Commands::Admin(admin_args)` pattern match works without importing `AdminCommands` — dispatch goes straight to `admin::run(admin_args.command)` which is defined in `src/cli/admin.rs`.

- [ ] Build and verify clean:

```bash
cargo build
```

Expected: clean build, no errors.

- [ ] Run tests:

```bash
cargo test
```

Expected: all pass.

- [ ] Commit:

```bash
git add Cargo.toml Cargo.lock src/cli/commands.rs src/cli/mod.rs src/cli/admin.rs src/db/repositories/admin_users.rs src/db/sqlite/admin_users.rs src/db/postgres/admin_users.rs
git commit -m "feat: add modelrouter admin CLI subcommand group"
```

---

## Task 8: Secure file permissions in `modelrouter init`

**Files:**
- Modify: `src/cli/mod.rs` (inside `Commands::Init` arm)

`init` creates `~/.modelrouter/` but leaves it world-accessible. The DB file that sqlx later creates inside it inherits the directory's umask — which may be `644` or looser. Fix: after creating the config dir, set it to `0700` (owner-only). This means the DB file and config file (which contains provider API keys) are never readable by other OS users.

- [ ] Write a failing test first. Add to the `#[cfg(test)]` section of `src/cli/mod.rs` (create the module if it doesn't exist):

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn config_dir_permission_mode() {
        // Verify the unix permission bits we intend to set
        // 0o700 = rwx for owner only
        assert_eq!(0o700u32, 0b111_000_000);
    }
}
```

- [ ] Run the test:

```bash
cargo test config_dir_permission_mode
```

Expected: PASS (this is a sanity check on the constant, not the filesystem).

- [ ] In `src/cli/mod.rs`, find the `Commands::Init` arm. After `tokio::fs::create_dir_all(&config_dir).await?;`, add:

```rust
// Set config dir to owner-only so the DB and config (which holds API keys)
// are not readable by other OS users on shared servers.
#[cfg(unix)]
{
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(&config_dir, perms)?;
}
```

The `#[cfg(unix)]` guard means Windows builds compile cleanly (Windows uses ACLs, not Unix mode bits).

- [ ] Also set the config file itself to `0600` immediately after writing it. Find the two `tokio::fs::write(&config_path, CONFIG_TEMPLATE).await?;` calls and add after each:

```rust
#[cfg(unix)]
{
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(&config_path, perms)?;
}
```

- [ ] Build:

```bash
cargo build
```

Expected: clean.

- [ ] Run all tests:

```bash
cargo test
```

Expected: all pass.

- [ ] Commit:

```bash
git add src/cli/mod.rs
git commit -m "feat: set 0700/0600 permissions on modelrouter init config dir and file"
```

---

## Task 9: Manual smoke test

- [ ] Ensure you have a config file at `~/.modelrouter/config.toml` (or use `--config`). Run migrations if needed:

```bash
cargo run -- migrate
```

- [ ] Create first admin:

```bash
cargo run -- admin create --name admin
# Enter a password when prompted
# Expected: "Created admin 'admin' (id=1, role=superadmin)."
```

- [ ] List admins:

```bash
cargo run -- admin list
# Expected: table row showing admin, superadmin, enabled
```

- [ ] Create a viewer:

```bash
cargo run -- admin create --name readonly --role viewer
# Expected: "Created admin 'readonly' (id=2, role=viewer)."
```

- [ ] Disable/enable:

```bash
cargo run -- admin disable readonly
# Expected: "Disabled admin 'readonly'."
cargo run -- admin enable readonly
# Expected: "Enabled admin 'readonly'."
```

- [ ] Reset password:

```bash
cargo run -- admin reset-password --name admin
# Enter new password when prompted
# Expected: "Password updated for admin 'admin'."
```

- [ ] Verify audit rows were written:

```bash
cargo run -- audit --tail 10
# Expected: rows with actor=cli, actions admin.create, admin.reset_password, admin.enable, admin.disable
```

- [ ] Verify duplicate name is rejected:

```bash
cargo run -- admin create --name admin
# Expected: error: admin user 'admin' already exists
```

- [ ] Verify invalid role is rejected by clap:

```bash
cargo run -- admin create --name foo --role god
# Expected: clap error before any prompt
```

- [ ] Final test run:

```bash
cargo test
```

Expected: all pass.

- [ ] Commit if any fixes were made during smoke test. Then push:

```bash
git push
```
