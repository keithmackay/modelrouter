# Local Setup Guide

This guide gets modelrouter running on your Mac alongside your existing Claude Code setup. After following it you'll have:

- modelrouter proxying your Claude Code requests to Anthropic
- Per-request cost tracking in a local SQLite database
- The admin dashboard showing usage and audit logs at `http://localhost:8080/admin`
- (Optional) Arize Phoenix receiving OTel traces for every request

---

## Prerequisites

- macOS (Apple Silicon or Intel)
- Rust toolchain — install via [rustup](https://rustup.rs) if not already present: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- An Anthropic API key (`sk-ant-...`)
- Claude Code installed and working (`claude` in your PATH)

---

## Step 1: Build the binary

```bash
cd /Users/Keith.MacKay/Projects/modelrouter

# Default build (SQLite, no OTel)
cargo build --release

# Or with OpenTelemetry support (needed for Step 6)
cargo build --release --features otel
```

The binary lands at `target/release/modelrouter`. Add it to your PATH for convenience:

```bash
# Add to ~/.zshrc or ~/.bashrc
export PATH="$PATH:/Users/Keith.MacKay/Projects/modelrouter/target/release"
```

Open a new shell or `source ~/.zshrc`.

---

## Step 2: Initialize config and database

```bash
# Create ~/.modelrouter/config.toml with defaults
modelrouter init

# Create ~/.modelrouter/router.db and run all migrations
modelrouter migrate
```

---

## Step 3: Configure the Anthropic provider

Edit `~/.modelrouter/config.toml`. The key sections:

```toml
[providers.anthropic]
api_key = "sk-ant-YOUR_KEY_HERE"
timeout_secs = 120

[routing]
default_provider = "anthropic"
default_model    = "claude-opus-4-5"

[routing.model_aliases]
# These aliases let Claude Code resolve its model names through modelrouter.
# Add whatever names Claude Code sends in the "model" field.
"claude-opus-4-5"              = "anthropic/claude-opus-4-5"
"claude-sonnet-4-5"            = "anthropic/claude-sonnet-4-5"
"claude-haiku-4-5"             = "anthropic/claude-haiku-4-5-20251001"
"claude-opus-4-6"              = "anthropic/claude-opus-4-6"
"claude-sonnet-4-6"            = "anthropic/claude-sonnet-4-6"

[auth]
# Used to sign admin dashboard JWTs. Change this to any random string.
jwt_secret = "change-me-to-something-random"
```

See [`config.example.toml`](../config.example.toml) for the full annotated reference.

---

## Step 4: Create your user account

Every API key in modelrouter maps to a user. Create one for yourself:

```bash
modelrouter user create --name keith
# Created user 'keith' (id=1)
# API key: mr-a1b2c3d4...
# Store this key securely — it cannot be retrieved later.
```

Copy the `mr-...` key. You'll use it as your `ANTHROPIC_API_KEY`.

Optionally set a budget so you can test budget enforcement:

```bash
modelrouter budget set --user keith --window monthly --limit-usd 50.0
```

---

## Step 5: Point Claude Code at modelrouter

Claude Code reads `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY` from the environment. Set them in your shell profile so every Claude Code session routes through modelrouter:

```bash
# Add to ~/.zshrc or ~/.bashrc
export ANTHROPIC_BASE_URL="http://localhost:8080"
export ANTHROPIC_API_KEY="mr-a1b2c3d4..."   # your modelrouter key from Step 4
```

Open a new shell (or `source ~/.zshrc`), then start modelrouter:

```bash
modelrouter serve
# Listening on http://127.0.0.1:8080
```

Run Claude Code normally — `claude` — and watch modelrouter's stdout. You'll see a log line for each request showing the model, user, tokens, and cost.

To verify tracking is working:

```bash
modelrouter report cost --user keith --window monthly --format table
```

---

## Step 6: (Optional) Connect Arize Phoenix for tracing

If you built with `--features otel`, you can send traces and metrics to [Arize Phoenix](https://docs.arize.com/phoenix).

**Start Phoenix:**

```bash
pip install arize-phoenix
phoenix serve
# Dashboard: http://localhost:6006
# OTLP:      localhost:4317
```

Or with Docker:

```bash
docker run -p 6006:6006 -p 4317:4317 arizephoenix/phoenix:latest
```

**Add to `~/.modelrouter/config.toml`:**

```toml
[telemetry]
enabled           = true
endpoint          = "http://localhost:4317"
service_name      = "modelrouter"
sample_ratio      = 1.0        # trace every request while exploring
slow_threshold_ms = 2000
```

Restart modelrouter and send a request through Claude Code. Open `http://localhost:6006` — the trace appears within a few seconds. Each span carries `user.id`, `model.canonical`, `provider.name`, `tokens.prompt`, `tokens.completion`, and `cost.usd` attributes.

---

## Step 7: Explore the admin dashboard

With modelrouter running, open `http://localhost:8080/admin` in your browser.

**Create an admin account** (first time only):

```bash
# modelrouter does not yet have a `admin create` CLI command.
# Use the admin API directly:
curl -s -X POST http://localhost:8080/admin/api/admins \
  -H "Content-Type: application/json" \
  -d '{"name": "keith", "password": "your-password", "role": "superadmin"}' | jq
```

> **Note:** The first admin creation endpoint is unprotected (no JWT required) only when no admin accounts exist yet. Subsequent creations require superadmin authentication.

Log in at `http://localhost:8080/admin/login` with those credentials. The dashboard shows:

- **Overview** — total requests, cost, and token usage
- **Users** — list, enable/disable, rotate API keys
- **Cost** — per-user spend chart
- **Audit log** — every request with model, user, tokens, cost, and status
- **Prompts** — stored prompt/completion pairs (if prompt logging is enabled)
- **Hooks** — registered hooks and their last execution status

---

## Step 8: (Optional) Set up OIDC SSO for the admin dashboard

If you'd rather log in with your Google (or other IdP) account instead of a password:

1. Create an OAuth2 app in [Google Cloud Console](https://console.cloud.google.com/apis/credentials). Set the **Authorized redirect URI** to `http://localhost:8080/admin/auth/oidc/callback`.

2. Add to `~/.modelrouter/config.toml`:

```toml
[oidc]
enabled             = true
issuer_url          = "https://accounts.google.com"
client_id           = "YOUR_CLIENT_ID.apps.googleusercontent.com"
client_secret       = "YOUR_CLIENT_SECRET"
redirect_uri        = "http://localhost:8080/admin/auth/oidc/callback"
allowed_emails      = ["your.email@gmail.com"]
auto_provision_role = "superadmin"
```

3. Restart modelrouter. Navigate to `http://localhost:8080/admin/auth/oidc/login` — you'll be sent to Google, then redirected back as an authenticated admin.

---

## Step 9: (Optional) Run as a background service

To keep modelrouter running without a terminal window:

```bash
modelrouter install-service
# Installed launchd plist at ~/Library/LaunchAgents/com.modelrouter.plist
# Run: launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.modelrouter.plist
```

Logs go to `~/Library/Logs/modelrouter.log`.

To uninstall:

```bash
modelrouter uninstall-service
```

---

## Quick reference

| Task | Command |
|------|---------|
| Start server | `modelrouter serve` |
| Check a user's spend | `modelrouter report cost --user keith --window monthly` |
| See all users | `modelrouter user list` |
| View budget rules | `modelrouter budget list` |
| Tail audit log | `modelrouter report audit` |
| View stored prompts | `modelrouter report prompts --user keith` |
| Admin dashboard | `open http://localhost:8080/admin` |
| Phoenix traces | `open http://localhost:6006` |

---

## Troubleshooting

**Claude Code sends requests but nothing appears in cost reports**

Check that `ANTHROPIC_BASE_URL` is set in the same shell running Claude Code: `echo $ANTHROPIC_BASE_URL`. If it's empty, the SDK is talking directly to Anthropic.

**`429 Budget exceeded` immediately**

Either the budget window just reset and there's a stale `spend_reset_at` timestamp, or the limit is set very low. Check: `modelrouter report cost --user keith --window monthly` to see actual spend, and `modelrouter budget list` to see the limit.

**Admin dashboard shows "Unauthorized"**

Your `mr_admin_session` cookie may have expired (default: 60 minutes). Log in again at `/admin/login`.

**OTel traces don't appear in Phoenix**

Confirm `telemetry.enabled = true` in config and that modelrouter was built with `--features otel` (`modelrouter --version` won't show this, but `cargo build --features otel` should have produced a binary in `target/release/`). Check that Phoenix's OTLP port (4317) is reachable: `nc -zv localhost 4317`.

**`cargo build` fails with "no space left on device"**

The Rust target directory can grow to 50+ GB. Run `cargo clean` to reclaim space, then rebuild.
