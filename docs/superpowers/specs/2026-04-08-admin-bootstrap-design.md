# modelrouter — Admin Bootstrap CLI

_Written: 2026-04-08_

---

## Overview

Add a `modelrouter admin` CLI subcommand group that allows operators to create, list, enable, disable, and reset the password of admin users directly against the database — no running server or JWT required. This solves the chicken-and-egg problem of first-time admin provisioning and provides a recovery path when admin credentials are lost.

---

## Commands

```
modelrouter admin create --name <name> [--role superadmin|viewer]
modelrouter admin list [--format table|csv|json]
modelrouter admin reset-password --name <name>
modelrouter admin enable <name>
modelrouter admin disable <name>
```

`enable` and `disable` take a positional `name` argument (matching the existing `modelrouter user enable <name>` pattern). All other subcommands use `--name` flags where the name is logically a named option rather than a positional target.

### Role values

Valid roles: `superadmin` and `viewer` — matching the constraint in the HTTP `create_admin` handler exactly. `--role` defaults to `superadmin`. This is intentional: the primary purpose of this command is bootstrapping the first operator who needs full access. Operators creating secondary or restricted accounts must pass `--role viewer` explicitly. The clap `value_parser` (typed `AdminRole` enum) enforces the allowed set before any DB access.

### Password input

- `create` prompts interactively twice (password + confirm). Mismatch aborts with `error: passwords do not match` — nothing is written to the DB.
- `reset-password` prompts once, **intentionally** — the operator is authenticated at the OS level (shell / `docker exec` access to the host machine). A second confirmation prompt adds no security value; if a typo occurs, the operator runs `reset-password` again.
- Uses the `rpassword` crate for hidden terminal input. Requires a TTY — the recovery path (`docker exec -it`) must use the `-t` flag to allocate a pseudo-TTY; without it, `rpassword` will error with a clear message.

---

## Recovery Path

```bash
# Local / native binary
modelrouter admin reset-password --name admin

# Docker
docker exec -it <container> /modelrouter admin reset-password --name admin
```

Connects directly to SQLite via the config file — no server process or JWT needed. Works whether the server is running or stopped.

---

## Architecture

### Modified files

| File | Change |
|---|---|
| `Cargo.toml` | Add `rpassword = "7"` to `[dependencies]` |
| `src/cli/commands.rs` | Add `AdminArgs`, `AdminCommands` to existing `Commands` enum |
| `src/cli/mod.rs` | Add `pub mod admin;` declaration only; `Commands::Admin` delegates immediately to `admin::run(...)` |
| `src/cli/admin.rs` | **New file** — owns all `Admin` command arms including DB bootstrap (load config, connect, migrate) |
| `src/db/repositories/admin_users.rs` | Add `update_password_hash(id: i64, hash: &str)` to `AdminUserRepository` trait |
| `src/db/sqlite/admin_users.rs` | Implement `update_password_hash` for `SqliteDb` |
| `src/db/postgres/admin_users.rs` | Implement `update_password_hash` for `PostgresDb` |

### No new DB schema

The existing `admin_users` table has all required columns. No migration needed.

### DB interaction pattern

Identical to existing `user` and `budget` commands:
1. Load config (`crate::config::load`)
2. Connect SQLite (`SqliteDb::connect`)
3. Run migrations (`run_migrations`)
4. Call repository trait methods

### New repository method

Add to `AdminUserRepository` trait:

```rust
async fn update_password_hash(&self, id: i64, hash: &str) -> anyhow::Result<()>;
```

SQLite implementation — note bind order: `hash` is bound first (positional `?1`), then `id` (`?2`):
```sql
UPDATE admin_users SET password_hash = ? WHERE id = ?
-- .bind(hash).bind(id)
```

### Role validation

`--role` is validated by clap via a typed enum (`AdminRole`) with `value_parser`, so invalid values are rejected before any code runs. This is idiomatic with the existing clap usage and avoids runtime role string checks.

### Audit log

All five mutating commands (`create`, `reset-password`, `enable`, `disable`) write an audit row via `AuditRepository::create` with a `NewAuditLogEntry`. The `list` command is read-only and does not write an audit row.

There are **4 mutating commands** (create, reset-password, enable, disable); `list` is read-only and does not write an audit row.

Fields:

| Field | Value |
|---|---|
| `actor_id` | `None` |
| `actor_name` | `"cli"` |
| `action` | `"admin.create"`, `"admin.reset_password"`, `"admin.enable"`, or `"admin.disable"` |
| `target` | `"admin:<id>"` |
| `before_json` | `None` — intentionally omitted for all commands; prior state is recoverable from prior audit rows |
| `after_json` | JSON — see per-command detail below |

`after_json` per command:
- `create`: `{"name": "...", "role": "..."}` — no password hash
- `reset-password`: `{"name": "..."}` — confirms which account was affected; no hash
- `enable`: `{"name": "...", "enabled": true}`
- `disable`: `{"name": "...", "enabled": false}`

---

## Output Format

`admin list` follows the existing CLI reporting pattern and supports `--format table|csv|json` (default: `table`). Columns: `ID`, `Name`, `Role`, `Status`, `Created At`. Password hashes are never printed.

Example table output:
```
   1  admin               superadmin  enabled   2026-04-08T22:30:00
   2  readonly            viewer      enabled   2026-04-08T22:35:00
```

---

## Error Handling

| Scenario | Behaviour |
|---|---|
| Name already exists on `create` | `error: admin user '<name>' already exists` |
| Name not found on `reset-password` / `enable` / `disable` | `error: admin user '<name>' not found` |
| Password confirm mismatch on `create` | `error: passwords do not match` — abort, nothing written |
| Invalid `--role` value | `error: role must be 'superadmin' or 'viewer'` |
| DB connection failure | Propagate `anyhow` error (same as all other CLI commands) |

---

## Example Session

```
$ modelrouter admin create --name admin
Password:
Confirm password:
Created admin 'admin' (id=1, role=superadmin).
Store this password securely — it cannot be retrieved later.

$ modelrouter admin list
   1  admin               superadmin  enabled   2026-04-08T22:30:00

$ modelrouter admin reset-password --name admin
New password:
Password updated for admin 'admin'.

$ modelrouter admin disable readonly
Disabled admin 'readonly'.
```

---

## Success Criteria

- [ ] `modelrouter admin create --name admin` creates a bcrypt-hashed superadmin row in `admin_users`
- [ ] `modelrouter admin create --name viewer --role viewer` creates a viewer row
- [ ] Invalid `--role` value is rejected before any DB write
- [ ] `modelrouter admin list` prints all admins with no password hashes; supports `--format`
- [ ] `modelrouter admin reset-password --name admin` updates `password_hash` via `update_password_hash`, writes audit row
- [ ] `modelrouter admin enable/disable <name>` toggles `enabled`, writes audit row
- [ ] Password mismatch on `create` aborts without DB write
- [ ] All four mutating commands write a `NewAuditLogEntry` with `actor_name = "cli"` via `AuditRepository::create`
- [ ] Works via `docker exec -it` (TTY allocated); errors clearly when `-t` is omitted
- [ ] `cargo test` passes
