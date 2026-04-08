# modelrouter — Admin Bootstrap CLI

_Written: 2026-04-08_

---

## Overview

Add a `modelrouter admin` CLI subcommand group that allows operators to create, list, enable, disable, and reset the password of admin users directly against the database — no running server or JWT required. This solves the chicken-and-egg problem of first-time admin provisioning and provides a recovery path when admin credentials are lost.

---

## Commands

```
modelrouter admin create --name <name> [--role superadmin|admin|viewer]
modelrouter admin list
modelrouter admin reset-password --name <name>
modelrouter admin enable --name <name>
modelrouter admin disable --name <name>
```

### Defaults

- `--role` defaults to `superadmin` when no admin users exist in the database yet (first-run bootstrap), and `admin` when at least one already exists. The operator can always override explicitly.

### Password input

- `create` and `reset-password` prompt for password interactively using hidden terminal input (no echo).
- `create` prompts twice (confirm); mismatch aborts with a clear error message.
- `reset-password` prompts once.
- Uses the `rpassword` crate for cross-platform hidden input.

---

## Recovery Path

```bash
# Local / native binary
modelrouter admin reset-password --name admin

# Docker
docker exec -it <container> /modelrouter admin reset-password --name admin
```

The command connects directly to SQLite via the config file — no server process or JWT needed. It works whether the server is running or stopped.

---

## Architecture

### New files

| File | Change |
|---|---|
| `src/cli/commands.rs` | Add `AdminArgs`, `AdminCommands` to `Commands` enum |
| `src/cli/mod.rs` | Add `Commands::Admin` match arm |

### No new DB schema

The existing `admin_users` table has all required columns (`name`, `password_hash`, `role`, `enabled`, `created_at`). No migration needed.

### DB interaction pattern

Identical to existing `user` and `budget` commands:
1. Load config (`crate::config::load`)
2. Connect SQLite (`SqliteDb::connect`)
3. Run migrations (`run_migrations`)
4. Call `AdminUserRepository` methods

### New dependency

```toml
rpassword = "7"
```

Used only in the `admin create` and `admin reset-password` command arms. Not gated behind a feature flag — it's a small, well-maintained crate with no native library dependencies.

---

## Audit Trail

`create` and `reset-password` write to the `audit_log` table:

| Field | Value |
|---|---|
| `actor_id` | `NULL` |
| `actor_name` | `"cli"` |
| `action` | `"admin.create"` or `"admin.reset_password"` |
| `target` | `"admin:<id>"` |
| `new_value` | `{"name": "...", "role": "..."}` (no password hash) |

This matches the pattern used by system-initiated audit events.

---

## Error Handling

| Scenario | Behaviour |
|---|---|
| Name already exists on `create` | Error: `admin user '<name>' already exists` |
| Name not found on `reset-password` / `enable` / `disable` | Error: `admin user '<name>' not found` |
| Password confirm mismatch | Error: `passwords do not match` — abort, nothing written |
| Invalid role string | Error: `role must be one of: superadmin, admin, viewer` |
| DB connection failure | Propagate anyhow error (same as all other CLI commands) |

---

## Example Output

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
```

---

## Success Criteria

- [ ] `modelrouter admin create --name admin` creates a bcrypt-hashed superadmin row
- [ ] `modelrouter admin list` prints all admin users (no password hashes)
- [ ] `modelrouter admin reset-password --name admin` updates hash, writes audit row
- [ ] `modelrouter admin enable/disable` toggle `enabled` flag
- [ ] Password confirm mismatch aborts without DB write
- [ ] Works via `docker exec` against the running container's DB
- [ ] `cargo test` passes (no new async tests required — DB interaction follows existing pattern)
