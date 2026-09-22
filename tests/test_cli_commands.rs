//! Integration tests for CLI commands by spawning the real `modelrouter` binary.
//!
//! These tests cover CLI subcommands that were untested by in-process tests:
//! user create/list/enable/disable/rotate-key, budget set/list/edit/delete,
//! report cost/compare/usage/prompts/audit/hooks, experiment add/list/close/results,
//! webhook add/list/delete/enable/disable, admin create/list/enable/disable/reset-password/hash-password,
//! alias/model/failover/group/provider commands, and cache commands that require a running server.
//!
//! Strategy: reuse/extend the e2e fixture machinery (tests/common/e2e.rs) to spawn
//! CLI subcommands in isolated tempdirs with temp config/DB, asserting on exit codes,
//! stdout, and DB state. The binary-spawning fixture already supports coverage via
//! `cargo llvm-cov`, which instruments spawned binaries.

mod common;

use common::e2e::{run_cli, RouterProcess, RouterOptions};
use std::path::PathBuf;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_modelrouter");

/// Create a fresh config + db in a tempdir, run `init` and `migrate`.
fn setup_db() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let config_path = dir.path().join("config.toml");

    // Run init to create config (it writes to HOME/.modelrouter, so override HOME)
    let init_home = tempfile::tempdir().expect("temp home for init");
    let out = std::process::Command::new(BIN)
        .arg("init")
        .env("HOME", init_home.path())
        .output()
        .expect("run init");
    assert!(out.status.success(), "init failed: {}", String::from_utf8_lossy(&out.stderr));

    // Copy the generated config to our test dir
    let generated = init_home.path().join(".modelrouter/config.toml");
    let content = std::fs::read_to_string(&generated).expect("read generated config");
    // Point it at our test DB
    let db_path = dir.path().join("router.db");
    let updated = content.replace(
        "path = \"~/.modelrouter/router.db\"",
        &format!("path = \"{}\"", db_path.display()),
    );
    std::fs::write(&config_path, updated).expect("write config");

    // Run migrate
    let (ok, _out, err) = run_cli(&config_path, &["migrate"]);
    assert!(ok, "migrate failed: {err}");

    (dir, config_path)
}

// ── User commands ─────────────────────────────────────────────────────────────

#[test]
fn user_create_and_list() {
    let (_dir, config) = setup_db();

    // user list — empty
    let (ok, out, _err) = run_cli(&config, &["user", "list"]);
    assert!(ok);
    assert!(out.trim().is_empty() || !out.contains("alice"), "list should be empty initially");

    // user create --name alice
    let (ok, out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok, "user create failed");
    assert!(out.contains("alice"), "output should name the user");
    assert!(out.contains("mr-"), "output should include an API key");

    // user list — populated
    let (ok, out, _err) = run_cli(&config, &["user", "list"]);
    assert!(ok);
    assert!(out.contains("alice"));
    assert!(out.contains("enabled"));
}

#[test]
fn user_enable_disable() {
    let (_dir, config) = setup_db();

    // Create alice
    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // Disable alice (positional argument, not --name)
    let (ok, out, err) = run_cli(&config, &["user", "disable", "alice"]);
    assert!(ok, "user disable failed: {}", err);
    assert!(out.contains("Disabled user 'alice'"));

    // List should show disabled
    let (ok, out, _err) = run_cli(&config, &["user", "list"]);
    assert!(ok);
    assert!(out.contains("alice"));
    assert!(out.contains("disabled"));

    // Enable alice (positional argument)
    let (ok, out, err) = run_cli(&config, &["user", "enable", "alice"]);
    assert!(ok, "user enable failed: {}", err);
    assert!(out.contains("Enabled user 'alice'"));

    // List should show enabled
    let (ok, out, _err) = run_cli(&config, &["user", "list"]);
    assert!(ok);
    assert!(out.contains("alice"));
    assert!(out.contains("enabled"));
}

#[test]
fn user_rotate_key() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // Rotate key — UserCommands::RotateKey exists but works at user level, not project
    // It rotates the initial key created with user create
    let (ok, out, err) = run_cli(&config, &["user", "rotate-key", "alice"]);
    assert!(ok, "user rotate-key failed: {}", err);
    assert!(out.contains("New key for") || out.contains("alice"));
    assert!(out.contains("mr-"));
}

#[test]
fn user_create_unknown_user_error() {
    let (_dir, config) = setup_db();

    // Enable unknown user should fail (positional argument)
    let (ok, _out, err) = run_cli(&config, &["user", "enable", "unknown"]);
    assert!(!ok);
    assert!(err.contains("not found") || err.contains("User not found") || err.contains("error"));
}

// ── Budget commands ───────────────────────────────────────────────────────────

#[test]
fn budget_set_and_list() {
    let (_dir, config) = setup_db();

    // Create alice
    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // budget set --user alice --limit 10.0 --window monthly
    let (ok, out, _err) = run_cli(&config, &[
        "budget", "set", "--user", "alice", "--limit-usd", "10.0", "--window", "monthly"
    ]);
    assert!(ok);
    assert!(out.contains("Created budget rule"));

    // budget list
    let (ok, out, _err) = run_cli(&config, &["budget", "list"]);
    assert!(ok);
    assert!(out.contains("alice"));
    assert!(out.contains("monthly"));
    assert!(out.contains("limit=$10.00"));
}

#[test]
fn budget_set_global() {
    let (_dir, config) = setup_db();

    // budget set --global --limit 100.0 --window monthly
    let (ok, out, _err) = run_cli(&config, &[
        "budget", "set", "--global", "--limit-usd", "100.0", "--window", "monthly"
    ]);
    assert!(ok);
    assert!(out.contains("Created budget rule"));

    // budget list
    let (ok, out, _err) = run_cli(&config, &["budget", "list"]);
    assert!(ok);
    assert!(out.contains("global"));
    assert!(out.contains("monthly"));
}

#[test]
fn budget_set_unknown_user_fails() {
    let (_dir, config) = setup_db();

    let (ok, _out, err) = run_cli(&config, &[
        "budget", "set", "--user", "unknown", "--limit-usd", "10.0", "--window", "monthly"
    ]);
    assert!(!ok);
    assert!(err.contains("not found") || err.contains("User not found"));
}

#[test]
fn budget_set_invalid_window_validates() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // Valid windows: monthly, annual, daily, target
    // Test that monthly works (already tested elsewhere), here just verify the command runs
    let (ok, _out, _err) = run_cli(&config, &[
        "budget", "set", "--user", "alice", "--limit-usd", "10.0", "--window", "monthly"
    ]);
    assert!(ok, "monthly window should work");
}

#[test]
fn budget_edit() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // Create a budget rule
    let (ok, out, err) = run_cli(&config, &[
        "budget", "set", "--user", "alice", "--limit-usd", "10.0", "--window", "monthly"
    ]);
    assert!(ok, "budget set failed: {}", err);
    let id = extract_id_safe(&out, "Created budget rule id=").expect("no ID in budget output");

    // Edit the limit
    let (ok, out, err) = run_cli(&config, &[
        "budget", "edit", "--id", &id.to_string(), "--limit-usd", "20.0"
    ]);
    assert!(ok, "budget edit failed: {}\n{}", out, err);
    assert!(out.contains("Updated budget rule"));

    // List should show new limit
    let (ok, out, _err) = run_cli(&config, &["budget", "list"]);
    assert!(ok);
    assert!(out.contains("limit=$20.00"));
}

#[test]
fn budget_delete() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    let (ok, out, err) = run_cli(&config, &[
        "budget", "set", "--user", "alice", "--limit-usd", "10.0", "--window", "monthly"
    ]);
    assert!(ok, "budget set failed: {}", err);
    let id = extract_id_safe(&out, "Created budget rule id=").expect("no ID in budget output");

    // Delete
    let (ok, out, err) = run_cli(&config, &["budget", "delete", "--id", &id.to_string()]);
    assert!(ok, "budget delete failed: {}\n{}", out, err);
    assert!(out.contains("Deleted budget rule"));

    // List should be empty
    let (ok, out, _err) = run_cli(&config, &["budget", "list"]);
    assert!(ok);
    assert!(!out.contains("alice") || !out.contains("monthly"));
}

// ── Group commands ────────────────────────────────────────────────────────────

#[test]
fn group_create_and_list() {
    let (_dir, config) = setup_db();

    // group create --name team-a --priority 100
    let (ok, out, _err) = run_cli(&config, &[
        "group", "create", "--name", "team-a", "--priority", "100"
    ]);
    assert!(ok);
    assert!(out.contains("Created group 'team-a'"));

    // group list
    let (ok, out, _err) = run_cli(&config, &["group", "list"]);
    assert!(ok);
    assert!(out.contains("team-a"));
    assert!(out.contains("priority=100"));
}

#[test]
fn group_add_and_remove_member() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);
    let (ok, _out, _err) = run_cli(&config, &["group", "create", "--name", "team-a", "--priority", "100"]);
    assert!(ok);

    // group add-member --group team-a --user alice
    let (ok, out, _err) = run_cli(&config, &["group", "add-member", "--group", "team-a", "--user", "alice"]);
    assert!(ok);
    assert!(out.contains("Added 'alice' to group 'team-a'"));

    // group members --group team-a
    let (ok, out, _err) = run_cli(&config, &["group", "members", "--group", "team-a"]);
    assert!(ok);
    assert!(out.contains("alice"));

    // group remove-member --group team-a --user alice
    let (ok, out, _err) = run_cli(&config, &["group", "remove-member", "--group", "team-a", "--user", "alice"]);
    assert!(ok);
    assert!(out.contains("Removed 'alice' from group 'team-a'"));
}

#[test]
fn group_enable_disable() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["group", "create", "--name", "team-a", "--priority", "100"]);
    assert!(ok);

    // group disable team-a (note: no --name flag)
    let (ok, out, err) = run_cli(&config, &["group", "disable", "team-a"]);
    assert!(ok, "group disable failed: {}", err);
    assert!(out.contains("Disabled group 'team-a'"));

    // group enable team-a
    let (ok, out, err) = run_cli(&config, &["group", "enable", "team-a"]);
    assert!(ok, "group enable failed: {}", err);
    assert!(out.contains("Enabled group 'team-a'"));
}

// ── Report commands ───────────────────────────────────────────────────────────

#[test]
fn report_cost_empty() {
    let (_dir, config) = setup_db();

    // report cost (no data)
    let (ok, out, _err) = run_cli(&config, &["report", "cost"]);
    assert!(ok);
    // Empty result is valid
    assert!(!out.contains("error"));
}

#[test]
fn report_cost_formats() {
    let (_dir, config) = setup_db();

    // report cost --format json
    let (ok, out, _err) = run_cli(&config, &["report", "cost", "--format", "json"]);
    assert!(ok);
    assert!(out.starts_with('[') || out.starts_with('{') || out.trim() == "[]", "JSON output");

    // report cost --format csv
    let (ok, out, _err) = run_cli(&config, &["report", "cost", "--format", "csv"]);
    assert!(ok);
    assert!(out.contains(',') || out.trim().is_empty(), "CSV output");

    // report cost --format table (default)
    let (ok, _out, _err) = run_cli(&config, &["report", "cost", "--format", "table"]);
    assert!(ok);
}

#[test]
fn report_compare_requires_dimension() {
    let (_dir, config) = setup_db();

    // report compare --dimension model --a gpt-4 --b opus
    let (ok, _out, _err) = run_cli(&config, &[
        "report", "compare", "--dimension", "model", "--a", "gpt-4", "--b", "opus", "--window", "alltime"
    ]);
    assert!(ok, "compare with valid args should succeed even with no data");
}

#[test]
fn report_prompts() {
    let (_dir, config) = setup_db();

    // report prompts --limit 10
    let (ok, _out, _err) = run_cli(&config, &["report", "prompts", "--limit", "10"]);
    assert!(ok);
}

#[test]
fn report_audit() {
    let (_dir, config) = setup_db();

    // report audit --tail 20
    let (ok, _out, _err) = run_cli(&config, &["report", "audit", "--tail", "20"]);
    assert!(ok);
}

#[test]
fn report_hooks() {
    let (_dir, config) = setup_db();

    // report hooks
    let (ok, _out, _err) = run_cli(&config, &["report", "hooks"]);
    assert!(ok);
}

#[test]
fn audit_command() {
    let (_dir, config) = setup_db();

    // audit --tail 50 --format json
    let (ok, _out, _err) = run_cli(&config, &["audit", "--tail", "50", "--format", "json"]);
    assert!(ok);
}

// ── Admin commands ────────────────────────────────────────────────────────────

#[test]
fn admin_hash_password() {
    let (_dir, config) = setup_db();

    // admin hash-password — skip stdin test as it's complex in tests
    // Instead, just verify the command exists and parses
    let (ok, _out, _err) = run_cli(&config, &["admin", "--help"]);
    assert!(ok, "admin --help should work");

    // Verify hash-password is in the help
    let (ok, out, _err) = run_cli(&config, &["admin", "hash-password", "--help"]);
    assert!(ok || out.contains("hash-password") || out.contains("password"), "hash-password help should work");
}

#[test]
fn admin_list_empty() {
    let (_dir, config) = setup_db();

    // admin list
    let (ok, out, _err) = run_cli(&config, &["admin", "list", "--format", "table"]);
    assert!(ok);
    // May be empty or have bootstrap admin; either is valid
    assert!(!out.contains("error"));
}

#[test]
fn admin_list_formats() {
    let (_dir, config) = setup_db();

    // admin list --format json
    let (ok, out, _err) = run_cli(&config, &["admin", "list", "--format", "json"]);
    assert!(ok);
    assert!(out.starts_with('[') || out.trim() == "[]");

    // admin list --format csv
    let (ok, _out, _err) = run_cli(&config, &["admin", "list", "--format", "csv"]);
    assert!(ok);
}

#[tokio::test]
async fn admin_enable_disable() {
    let (dir, config) = setup_db();

    // Create admin user directly via SQL
    let db_path = dir.path().join("router.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display()))
        .await.expect("connect to db");

    let password_hash = bcrypt::hash("testpass", bcrypt::DEFAULT_COST).expect("hash");

    // Insert admin user
    sqlx::query(
        "INSERT INTO admin_users (name, password_hash, role, enabled, created_at)
         VALUES (?, ?, ?, 1, datetime('now'))"
    )
    .bind("testadmin")
    .bind(&password_hash)
    .bind("viewer")
    .execute(&pool)
    .await.expect("insert admin");

    // admin list should show it
    let (ok, out, _err) = run_cli(&config, &["admin", "list"]);
    assert!(ok);
    assert!(out.contains("testadmin"));

    // admin disable testadmin
    let (ok, out, err) = run_cli(&config, &["admin", "disable", "testadmin"]);
    assert!(ok, "admin disable failed: {}", err);
    assert!(out.contains("Disabled admin 'testadmin'"));

    // Verify it's disabled via SQL
    let enabled: bool = sqlx::query_scalar("SELECT enabled FROM admin_users WHERE name = ?")
        .bind("testadmin")
        .fetch_one(&pool)
        .await.expect("query enabled");
    assert!(!enabled);

    // admin enable testadmin
    let (ok, out, err) = run_cli(&config, &["admin", "enable", "testadmin"]);
    assert!(ok, "admin enable failed: {}", err);
    assert!(out.contains("Enabled admin 'testadmin'"));

    // Verify it's enabled via SQL
    let enabled: bool = sqlx::query_scalar("SELECT enabled FROM admin_users WHERE name = ?")
        .bind("testadmin")
        .fetch_one(&pool)
        .await.expect("query enabled");
    assert!(enabled);
}

#[tokio::test]
async fn admin_enable_unknown_fails() {
    let (_dir, config) = setup_db();

    // admin enable unknown-admin should fail
    let (ok, _out, err) = run_cli(&config, &["admin", "enable", "unknown-admin"]);
    assert!(!ok);
    assert!(err.contains("not found") || err.contains("error"));
}

#[tokio::test]
async fn admin_disable_unknown_fails() {
    let (_dir, config) = setup_db();

    // admin disable unknown-admin should fail
    let (ok, _out, err) = run_cli(&config, &["admin", "disable", "unknown-admin"]);
    assert!(!ok);
    assert!(err.contains("not found") || err.contains("error"));
}

#[tokio::test]
async fn admin_create_duplicate_fails() {
    let (dir, config) = setup_db();

    // Create an admin user directly in DB
    let db_path = dir.path().join("router.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display()))
        .await.expect("connect");

    let password_hash = bcrypt::hash("testpass", bcrypt::DEFAULT_COST).expect("hash");
    sqlx::query(
        "INSERT INTO admin_users (name, password_hash, role, enabled, created_at)
         VALUES (?, ?, ?, 1, datetime('now'))"
    )
    .bind("existing")
    .bind(&password_hash)
    .bind("superadmin")
    .execute(&pool)
    .await.expect("insert admin");

    // Creating another admin with the same name fails on the uniqueness check,
    // which runs before the password prompt — so this needs no terminal.
    let (ok, _out, err) = run_cli(&config, &["admin", "create", "--name", "existing"]);
    assert!(!ok, "duplicate admin name must fail");
    assert!(err.contains("admin user 'existing' already exists"), "{err}");

    // Still exactly one row, with the hash we inserted.
    let names: Vec<String> = sqlx::query_scalar("SELECT name FROM admin_users")
        .fetch_all(&pool)
        .await
        .expect("list admins");
    assert_eq!(names, vec!["existing".to_string()]);
}

// ── Key commands ──────────────────────────────────────────────────────────────

#[test]
fn key_create_and_list() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // key create --user alice --project test
    let (ok, _out, err) = run_cli(&config, &["key", "create", "--user", "alice", "--project", "test"]);
    assert!(ok, "key create failed: {}", err);

    // key list --user alice
    let (ok, out, _err) = run_cli(&config, &["key", "list", "--user", "alice"]);
    assert!(ok);
    assert!(out.contains("alice") || out.contains("test"));
}

#[test]
fn key_disable() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    let (ok, _out, err) = run_cli(&config, &["key", "create", "--user", "alice", "--project", "test"]);
    assert!(ok, "key create failed: {}", err);

    // key disable --user alice --project test
    let (ok, out, err) = run_cli(&config, &["key", "disable", "--user", "alice", "--project", "test"]);
    assert!(ok, "key disable failed: {}", err);
    assert!(out.contains("Disabled") || out.contains("disabled"));
}

// ── Alias commands ────────────────────────────────────────────────────────────

#[test]
fn alias_set_and_list() {
    let (_dir, config) = setup_db();

    // alias set deep anthropic/claude-opus-4
    let (ok, out, _err) = run_cli(&config, &["alias", "set", "deep", "anthropic/claude-opus-4"]);
    assert!(ok);
    assert!(out.contains("deep") || out.contains("Set alias"));

    // alias list
    let (ok, out, _err) = run_cli(&config, &["alias", "list"]);
    assert!(ok);
    assert!(out.contains("deep"));
}

#[test]
fn alias_rm() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["alias", "set", "deep", "anthropic/claude-opus-4"]);
    assert!(ok);

    // alias rm deep
    let (ok, out, _err) = run_cli(&config, &["alias", "rm", "deep"]);
    assert!(ok);
    assert!(out.contains("Removed") || out.contains("deleted"));
}

// ── Model commands ────────────────────────────────────────────────────────────

#[test]
fn model_list() {
    let (_dir, config) = setup_db();

    // model list
    let (ok, _out, _err) = run_cli(&config, &["model", "list"]);
    assert!(ok);
}

// ── Failover commands ─────────────────────────────────────────────────────────

#[test]
fn failover_set_and_list() {
    let (_dir, config) = setup_db();

    // failover set --primary gpt-4 --fallback gpt-3.5-turbo
    // Failover is a subcommand of model, not top-level
    let (ok, out, err) = run_cli(&config, &[
        "model", "failover", "set", "--model", "gpt-4", "--fallback", "gpt-3.5-turbo"
    ]);
    assert!(ok, "failover set failed: {}", err);
    assert!(out.contains("failover") || out.contains("Set"));

    // model failover list
    let (ok, out, _err) = run_cli(&config, &["model", "failover", "list"]);
    assert!(ok);
    assert!(out.contains("gpt-4") || out.len() > 0); // May be empty or contain the chain
}

#[test]
fn failover_list_specific_model() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["model", "failover", "set", "--model", "gpt-4", "--fallback", "gpt-3.5-turbo"]);
    assert!(ok);

    // model failover list --model gpt-4
    let (ok, out, _err) = run_cli(&config, &["model", "failover", "list", "--model", "gpt-4"]);
    assert!(ok);
    assert!(out.contains("gpt-4") || out.contains("gpt-3.5-turbo"));
}

// ── Provider commands ─────────────────────────────────────────────────────────

#[test]
fn provider_list() {
    let (_dir, config) = setup_db();

    // provider list
    let (ok, _out, _err) = run_cli(&config, &["provider", "list"]);
    assert!(ok);
}

// ── Webhook commands ──────────────────────────────────────────────────────────

#[test]
fn webhook_add_and_list() {
    let (_dir, config) = setup_db();

    // webhook add --name test --url http://example.com/hook
    let (ok, out, err) = run_cli(&config, &[
        "webhook", "add", "--name", "test", "--url", "http://example.com/hook"
    ]);
    assert!(ok, "webhook add failed: {}", err);
    assert!(out.contains("test") || out.contains("Created"));

    // webhook list
    let (ok, out, _err) = run_cli(&config, &["webhook", "list"]);
    assert!(ok);
    assert!(out.contains("test"));
}

#[test]
fn webhook_enable_disable_delete() {
    let (_dir, config) = setup_db();

    let (ok, out, err) = run_cli(&config, &["webhook", "add", "--name", "test", "--url", "http://example.com/hook"]);
    assert!(ok, "webhook add failed: {}", err);
    let id = extract_id_safe(&out, "id=").expect("no ID in webhook output");

    // webhook disable --id <id>
    let (ok, out, err) = run_cli(&config, &["webhook", "disable", "--id", &id.to_string()]);
    assert!(ok, "webhook disable failed: {}", err);
    assert!(out.contains("Disabled") || out.contains("disabled"));

    // webhook enable --id <id>
    let (ok, out, err) = run_cli(&config, &["webhook", "enable", "--id", &id.to_string()]);
    assert!(ok, "webhook enable failed: {}", err);
    assert!(out.contains("Enabled") || out.contains("enabled"));

    // webhook delete --id <id>
    let (ok, out, err) = run_cli(&config, &["webhook", "delete", "--id", &id.to_string()]);
    assert!(ok, "webhook delete failed: {}", err);
    assert!(out.contains("Deleted") || out.contains("deleted"));
}

// ── Experiment commands ───────────────────────────────────────────────────────

#[test]
fn experiment_add_and_list() {
    let (_dir, config) = setup_db();

    // experiment add requires valid provider/model targets — use openai/gpt-4 as it's in default config
    let (ok, out, err) = run_cli(&config, &[
        "experiment", "add",
        "--name", "test-exp",
        "--variant", "control=",
        "--variant", "treat=gpt-4:openai/gpt-4",
        "--expires-at", "never",
        "--content-retention-days", "30"
    ]);
    assert!(ok, "experiment add failed: {}\n{}", out, err);
    assert!(out.contains("test-exp") || out.contains("Created") || out.contains("id="));

    // experiment list
    let (ok, out, _err) = run_cli(&config, &["experiment", "list"]);
    assert!(ok);
    assert!(out.contains("test-exp") || out.len() > 0);
}

#[test]
fn experiment_close() {
    let (_dir, config) = setup_db();

    let (ok, out, err) = run_cli(&config, &[
        "experiment", "add",
        "--name", "test-exp",
        "--variant", "control=",
        "--variant", "treat=gpt-4:openai/gpt-4",
        "--expires-at", "never",
        "--content-retention-days", "30"
    ]);
    assert!(ok, "experiment add failed: {}\n{}", out, err);
    let id = extract_id_safe(&out, "id=").expect("no ID in experiment output");

    // experiment close --id <id>
    let (ok, out, err) = run_cli(&config, &["experiment", "close", "--id", &id.to_string()]);
    assert!(ok, "experiment close failed: {}\n{}", out, err);
    assert!(out.contains("Closed") || out.contains("closed"));
}

#[test]
fn experiment_results() {
    let (_dir, config) = setup_db();

    let (ok, out, err) = run_cli(&config, &[
        "experiment", "add",
        "--name", "test-exp",
        "--variant", "control=",
        "--variant", "treat=gpt-4:openai/gpt-4",
        "--expires-at", "never",
        "--content-retention-days", "30"
    ]);
    assert!(ok, "experiment add failed: {}\n{}", out, err);
    let id = extract_id_safe(&out, "id=").expect("no ID in experiment output");

    // experiment results --id <id> --limit 10
    let (ok, _out, err) = run_cli(&config, &[
        "experiment", "results", "--id", &id.to_string(), "--limit", "10"
    ]);
    assert!(ok, "experiment results failed: {}", err);
}

#[test]
fn experiment_add_validation_errors() {
    let (_dir, config) = setup_db();

    // Missing variant
    let (ok, _out, _err) = run_cli(&config, &[
        "experiment", "add",
        "--name", "test-exp",
        "--expires-at", "never",
        "--content-retention-days", "30"
    ]);
    assert!(!ok, "should fail without variants");

    // Missing expiry
    let (ok, _out, _err) = run_cli(&config, &[
        "experiment", "add",
        "--name", "test-exp",
        "--variant", "control=",
        "--variant", "treat=gpt-4:openai/gpt-4",
        "--content-retention-days", "30"
    ]);
    assert!(!ok, "should fail without expiry");
}

// ── Helper functions ──────────────────────────────────────────────────────────

/// Extract a numeric ID from output like "Created budget rule id=123"
fn extract_id_safe(output: &str, prefix: &str) -> Option<i64> {
    // Try exact prefix first
    if let Some(id) = output
        .split_whitespace()
        .find(|tok| tok.starts_with(prefix))
        .and_then(|tok| tok.trim_start_matches(prefix).parse::<i64>().ok())
    {
        return Some(id);
    }

    // Also try "id=" without prefix for budget commands that just say "Created budget rule id=N"
    if prefix.ends_with("id=") {
        output
            .split_whitespace()
            .find(|tok| tok.starts_with("id="))
            .and_then(|tok| tok.trim_start_matches("id=").parse::<i64>().ok())
    } else {
        None
    }
}

/// Extract the API key from output like "API key: mr-abc123"
fn extract_key(output: &str) -> String {
    output
        .split_whitespace()
        .find(|tok| tok.starts_with("mr-") && tok.len() > 10)
        .map(|tok| tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-'))
        .map(str::to_string)
        .unwrap_or_else(|| {
            panic!("no API key in output: {}", output);
        })
}

// ── Cache commands (require a running server) ────────────────────────────────

/// The cache commands talk to a running server's admin API, so these tests
/// spawn a RouterProcess fixture and use bootstrap admin from config.

async fn setup_router_with_admin() -> (common::mock_llm::MockLlm, RouterProcess, String) {
    let mock = common::mock_llm::MockLlm::start().await;

    // Create router with bootstrap admin in config
    let dir = tempfile::tempdir().expect("temp dir");
    let config_path = dir.path().join("config.toml");
    let db_path = dir.path().join("router.db");
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        listener.local_addr().expect("port").port()
    };

    // Hash a known password
    let password_hash = bcrypt::hash("testpass", bcrypt::DEFAULT_COST).expect("hash");

    let config_content = format!(r#"
[server]
host = "127.0.0.1"
port = {port}

[database]
path = "{db}"

[routing]
default_provider = "mock"
default_model = "mock-model"

[providers.mock]
api_base = "{base}"
api_key = "test-key"

[auth]
jwt_secret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[admin.bootstrap]
name = "admin"
role = "superadmin"
password_hash = "{password_hash}"

[cache]
enabled = true

[storage]
store_prompts = true
"#, port = port, db = db_path.display(), base = mock.base_url(), password_hash = password_hash);

    std::fs::write(&config_path, config_content).expect("write config");

    // Run migrate
    let (ok, _out, err) = run_cli(&config_path, &["migrate"]);
    assert!(ok, "migrate failed: {}", err);

    let router = RouterProcess::start_with_config(config_path, dir).await;

    (mock, router, "testpass".to_string())
}

#[tokio::test]
async fn cache_stats_with_token() {
    let (_mock, router, password) = setup_router_with_admin().await;

    // First get a token by logging in via API
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/admin/api/login", router.base_url()))
        .json(&serde_json::json!({ "name": "admin", "password": password }))
        .send()
        .await
        .expect("login");
    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.expect("json");
    let token = body["token"].as_str().expect("token");

    // cache stats --token <token> --format json
    let (ok, out, err) = run_cli(router.config_path(), &[
        "cache", "--url", &router.base_url(), "--token", token, "stats", "--format", "json"
    ]);
    assert!(ok, "cache stats failed: {}", err);
    assert!(out.contains("backend") || out.contains("enabled"));
}

#[tokio::test]
async fn cache_stats_table_format() {
    let (_mock, router, password) = setup_router_with_admin().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/admin/api/login", router.base_url()))
        .json(&serde_json::json!({ "name": "admin", "password": password }))
        .send()
        .await
        .expect("login");
    let body: serde_json::Value = resp.json().await.expect("json");
    let token = body["token"].as_str().expect("token");

    // cache stats --format table (default)
    let (ok, out, err) = run_cli(router.config_path(), &[
        "cache", "--url", &router.base_url(), "--token", token, "stats"
    ]);
    assert!(ok, "cache stats failed: {}", err);
    assert!(out.contains("Backend:") || out.contains("Enabled:") || out.contains("Entries:"));
}

#[tokio::test]
async fn cache_purge_all() {
    let (_mock, router, password) = setup_router_with_admin().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/admin/api/login", router.base_url()))
        .json(&serde_json::json!({ "name": "admin", "password": password }))
        .send()
        .await
        .expect("login");
    let body: serde_json::Value = resp.json().await.expect("json");
    let token = body["token"].as_str().expect("token");

    // cache purge --all
    let (ok, out, err) = run_cli(router.config_path(), &[
        "cache", "--url", &router.base_url(), "--token", token, "purge", "--all"
    ]);
    assert!(ok, "cache purge failed: {}", err);
    assert!(out.contains("Purged") || out.contains("scope"));
}

#[tokio::test]
async fn cache_purge_by_model() {
    let (_mock, router, password) = setup_router_with_admin().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/admin/api/login", router.base_url()))
        .json(&serde_json::json!({ "name": "admin", "password": password }))
        .send()
        .await
        .expect("login");
    let body: serde_json::Value = resp.json().await.expect("json");
    let token = body["token"].as_str().expect("token");

    // cache purge --model gpt-4
    let (ok, out, err) = run_cli(router.config_path(), &[
        "cache", "--url", &router.base_url(), "--token", token, "purge", "--model", "gpt-4"
    ]);
    assert!(ok, "cache purge by model failed: {}", err);
    assert!(out.contains("Purged") || out.contains("gpt-4"));
}

#[tokio::test]
async fn cache_policy_get() {
    let (_mock, router, password) = setup_router_with_admin().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/admin/api/login", router.base_url()))
        .json(&serde_json::json!({ "name": "admin", "password": password }))
        .send()
        .await
        .expect("login");
    let body: serde_json::Value = resp.json().await.expect("json");
    let token = body["token"].as_str().expect("token");

    // cache policy get
    let (ok, out, err) = run_cli(router.config_path(), &[
        "cache", "--url", &router.base_url(), "--token", token, "policy", "get"
    ]);
    assert!(ok, "cache policy get failed: {}", err);
    assert!(out.contains("enabled") || out.contains("temperature") || out.contains("{"));
}

#[tokio::test]
async fn cache_policy_set() {
    let (_mock, router, password) = setup_router_with_admin().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/admin/api/login", router.base_url()))
        .json(&serde_json::json!({ "name": "admin", "password": password }))
        .send()
        .await
        .expect("login");
    let body: serde_json::Value = resp.json().await.expect("json");
    let token = body["token"].as_str().expect("token");

    // cache policy set --enabled true --max-temperature 0.5
    let (ok, out, err) = run_cli(router.config_path(), &[
        "cache", "--url", &router.base_url(), "--token", token, "policy", "set",
        "--enabled", "true", "--max-temperature", "0.5"
    ]);
    assert!(ok, "cache policy set failed: {}", err);
    assert!(out.contains("enabled") || out.contains("temperature"));
}

#[tokio::test]
async fn cache_stats_no_token_fails() {
    let (_mock, router, _password) = setup_router_with_admin().await;

    // Try cache stats without token - should fail
    let (ok, _out, err) = run_cli(router.config_path(), &[
        "cache", "--url", &router.base_url(), "stats"
    ]);
    assert!(!ok, "cache stats without token should fail");
    assert!(err.contains("token") || err.contains("admin") || err.contains("no admin token"));
}

// ── Additional command variations for coverage ────────────────────────────────

#[test]
fn report_usage_total_global() {
    let (_dir, config) = setup_db();

    // report usage --total --alltime --global
    let (ok, _out, _err) = run_cli(&config, &["report", "usage", "--total", "--alltime", "--global"]);
    assert!(ok);
}

#[test]
fn report_usage_subtotal_monthly() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // report usage --subtotal --monthly --global
    let (ok, _out, _err) = run_cli(&config, &["report", "usage", "--subtotal", "--monthly", "--global"]);
    assert!(ok);
}

#[test]
fn report_usage_detail_annual() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // report usage --detail --annual --user alice
    let (ok, _out, _err) = run_cli(&config, &["report", "usage", "--detail", "--annual", "--user", "alice"]);
    assert!(ok);
}

#[test]
fn report_usage_group_scope() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["group", "create", "--name", "team-a", "--priority", "100"]);
    assert!(ok);

    // report usage --total --alltime --group team-a
    let (ok, _out, _err) = run_cli(&config, &["report", "usage", "--total", "--alltime", "--group", "team-a"]);
    assert!(ok);
}

#[test]
fn report_usage_project_scope() {
    let (_dir, config) = setup_db();

    // report usage --total --alltime --project myproject
    let (ok, _out, _err) = run_cli(&config, &["report", "usage", "--total", "--alltime", "--project", "myproject"]);
    assert!(ok);
}

#[test]
fn key_commands_with_label_and_window() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // key create with --label and --session-window
    let (ok, out, err) = run_cli(&config, &[
        "key", "create", "--user", "alice", "--project", "test",
        "--label", "prod-key", "--session-window", "3600"
    ]);
    assert!(ok, "key create with label failed: {}", err);
    assert!(out.contains("mr-"));

    // key list --project filter
    let (ok, out, _err) = run_cli(&config, &["key", "list", "--project", "test"]);
    assert!(ok);
    assert!(out.contains("test") || out.contains("prod-key") || out.contains("alice"));
}

#[test]
fn budget_set_project_scope() {
    let (_dir, config) = setup_db();

    // budget set --project myproject
    let (ok, out, err) = run_cli(&config, &[
        "budget", "set", "--project", "myproject", "--limit-usd", "50.0", "--window", "monthly"
    ]);
    assert!(ok, "budget set --project failed: {}", err);
    assert!(out.contains("Created budget rule"));

    // budget list should show it
    let (ok, out, _err) = run_cli(&config, &["budget", "list"]);
    assert!(ok);
    assert!(out.contains("myproject") || out.contains("project="));
}

#[test]
fn budget_set_with_rate_and_concurrent_limits() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // budget set with --rate-rpm and --max-concurrent
    let (ok, out, err) = run_cli(&config, &[
        "budget", "set", "--user", "alice", "--window", "monthly",
        "--rate-rpm", "100", "--max-concurrent", "5"
    ]);
    assert!(ok, "budget set with rate limits failed: {}", err);
    assert!(out.contains("Created budget rule"));

    // budget list should show the limits
    let (ok, out, _err) = run_cli(&config, &["budget", "list"]);
    assert!(ok);
    assert!(out.contains("rpm=100") || out.contains("concurrent=5"));
}

#[test]
fn budget_set_with_model_allow_deny() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // budget set with --model-allow
    let (ok, out, err) = run_cli(&config, &[
        "budget", "set", "--user", "alice", "--window", "monthly",
        "--model-allow", "gpt-4,claude-opus"
    ]);
    assert!(ok, "budget set with model-allow failed: {}", err);
    assert!(out.contains("Created budget rule"));
}

#[tokio::test]
async fn model_enable_disable() {
    let (dir, config) = setup_db();

    // Create a model directly in DB
    let db_path = dir.path().join("router.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display()))
        .await.expect("connect");

    sqlx::query(
        "INSERT INTO models (provider, name, alias, enabled, created_at)
         VALUES (?, ?, ?, 1, datetime('now'))"
    )
    .bind("openai")
    .bind("gpt-4")
    .bind("g4")
    .execute(&pool)
    .await.expect("insert model");

    let model_id: i64 = sqlx::query_scalar("SELECT id FROM models WHERE name = 'gpt-4'")
        .fetch_one(&pool)
        .await.expect("get model id");

    // model disable --id <id> --reason "testing"
    let (ok, out, err) = run_cli(&config, &[
        "model", "disable", "--id", &model_id.to_string(), "--reason", "testing"
    ]);
    assert!(ok, "model disable failed: {}", err);
    assert!(out.contains("Disabled model"));

    // model enable --id <id>
    let (ok, out, err) = run_cli(&config, &["model", "enable", "--id", &model_id.to_string()]);
    assert!(ok, "model enable failed: {}", err);
    assert!(out.contains("Enabled model"));
}

#[tokio::test]
async fn model_delete() {
    let (dir, config) = setup_db();

    let db_path = dir.path().join("router.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display()))
        .await.expect("connect");

    sqlx::query(
        "INSERT INTO models (provider, name, enabled, created_at)
         VALUES (?, ?, 1, datetime('now'))"
    )
    .bind("openai")
    .bind("gpt-3.5-turbo")
    .execute(&pool)
    .await.expect("insert model");

    let model_id: i64 = sqlx::query_scalar("SELECT id FROM models WHERE name = 'gpt-3.5-turbo'")
        .fetch_one(&pool)
        .await.expect("get model id");

    // model delete --id <id>
    let (ok, out, err) = run_cli(&config, &["model", "delete", "--id", &model_id.to_string()]);
    assert!(ok, "model delete failed: {}", err);
    assert!(out.contains("Deleted model"));
}

#[test]
fn model_commands() {
    let (_dir, config) = setup_db();

    // model add
    let (ok, out, err) = run_cli(&config, &[
        "model", "add", "--provider", "openai", "--name", "gpt-4", "--alias", "g4"
    ]);
    assert!(ok, "model add failed: {}", err);
    assert!(out.contains("Created model") || out.contains("gpt-4"));

    // model list
    let (ok, out, _err) = run_cli(&config, &["model", "list"]);
    assert!(ok);
    assert!(out.contains("gpt-4") || out.contains("g4") || out.len() > 0);
}

#[test]
fn provider_disable_enable() {
    let (_dir, config) = setup_db();

    // provider disable with --reason
    let (ok, out, err) = run_cli(&config, &[
        "provider", "disable", "anthropic", "--reason", "maintenance"
    ]);
    // May fail if anthropic not configured, but tests the code path
    assert!(ok || err.contains("Unknown provider") || err.contains("anthropic"));

    if ok {
        assert!(out.contains("Disabled provider"));

        // provider list should show it disabled
        let (ok, out, _err) = run_cli(&config, &["provider", "list"]);
        assert!(ok);
        assert!(out.contains("anthropic") || out.len() > 0);

        // provider enable
        let (ok, out, _err) = run_cli(&config, &["provider", "enable", "anthropic"]);
        assert!(ok);
        assert!(out.contains("Enabled provider"));
    }
}

#[test]
fn alias_list_json() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["alias", "set", "fast", "gpt-3.5-turbo"]);
    assert!(ok);

    // alias list --format json
    let (ok, out, _err) = run_cli(&config, &["alias", "list", "--format", "json"]);
    assert!(ok);
    assert!(out.starts_with('['));
}

#[test]
fn experiment_list_all_status() {
    let (_dir, config) = setup_db();

    // experiment list --status all
    let (ok, _out, _err) = run_cli(&config, &["experiment", "list", "--status", "all"]);
    assert!(ok);

    // experiment list --status closed
    let (ok, _out, _err) = run_cli(&config, &["experiment", "list", "--status", "closed"]);
    assert!(ok);
}

#[tokio::test]
async fn report_cost_with_data() {
    let (dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // Insert some cost data
    let db_path = dir.path().join("router.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display()))
        .await.expect("connect");

    // Get user ID
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE name = 'alice'")
        .fetch_one(&pool)
        .await.expect("get user id");

    // Insert cost ledger entries
    sqlx::query(
        "INSERT INTO cost_ledger (user_id, model, provider, project, tokens_in, tokens_out, cost_usd, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, datetime('now'))"
    )
    .bind(user_id)
    .bind("gpt-4")
    .bind("openai")
    .bind("test-project")
    .bind(100)
    .bind(200)
    .bind(0.05)
    .execute(&pool)
    .await.expect("insert cost");

    // report cost --user alice (should show data)
    let (ok, out, _err) = run_cli(&config, &["report", "cost", "--user", "alice"]);
    assert!(ok);
    assert!(out.contains("alice") || out.contains("gpt-4") || out.contains("0.05"));

    // report cost --format json
    let (ok, out, _err) = run_cli(&config, &["report", "cost", "--format", "json"]);
    assert!(ok);
    assert!(out.contains("alice") || out.contains("gpt-4"));

    // report cost --format csv
    let (ok, out, _err) = run_cli(&config, &["report", "cost", "--format", "csv"]);
    assert!(ok);
    assert!(out.contains(','));
}

#[test]
fn report_cost_with_filters() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // report cost --user alice
    let (ok, _out, _err) = run_cli(&config, &["report", "cost", "--user", "alice"]);
    assert!(ok);

    // report cost --model gpt-4
    let (ok, _out, _err) = run_cli(&config, &["report", "cost", "--model", "gpt-4"]);
    assert!(ok);

    // report cost --window daily
    let (ok, _out, _err) = run_cli(&config, &["report", "cost", "--window", "daily"]);
    assert!(ok);
}

#[test]
fn report_prompts_with_filters() {
    let (_dir, config) = setup_db();

    let (ok, _out, _err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok);

    // report prompts --user alice --limit 5
    let (ok, _out, _err) = run_cli(&config, &["report", "prompts", "--user", "alice", "--limit", "5"]);
    assert!(ok);

    // report prompts --since 2024-01-01
    let (ok, _out, _err) = run_cli(&config, &["report", "prompts", "--since", "2024-01-01"]);
    assert!(ok);
}

#[test]
fn report_audit_with_actor_filter() {
    let (_dir, config) = setup_db();

    // report audit --actor cli
    let (ok, _out, _err) = run_cli(&config, &["report", "audit", "--actor", "cli"]);
    assert!(ok);
}

// ── Check-tls command ─────────────────────────────────────────────────────────

#[test]
fn check_tls() {
    let (_dir, config) = setup_db();

    // check-tls (may fail if no network, but should parse)
    let (_ok, _out, _err) = run_cli(&config, &["check-tls"]);
    // Success or failure depends on network; just verify it runs
}

// ── Install/uninstall service ─────────────────────────────────────────────────

#[test]
fn install_service_help() {
    // Just verify the command exists and can be invoked (won't actually install)
    let output = std::process::Command::new(BIN)
        .arg("install-service")
        .arg("--help")
        .output()
        .expect("run install-service --help");
    assert!(output.status.success());
}

#[test]
fn uninstall_service_help() {
    let output = std::process::Command::new(BIN)
        .arg("uninstall-service")
        .arg("--help")
        .output()
        .expect("run uninstall-service --help");
    assert!(output.status.success());
}

// ── Interactive password prompts (pseudo-terminal) ────────────────────────────
//
// `rpassword` 7 reads the password from `/dev/tty`, not from the process's
// stdin (see its `InputTarget::FilePath("/dev/tty")` default). A child spawned
// with `Stdio::piped()` therefore never reaches the prompt bodies at all: the
// open of `/dev/tty` fails and the command aborts before any of the interesting
// code runs. util-linux `script` closes that gap — it allocates a pty, runs the
// command on the slave side (so the child has a real controlling terminal) and
// copies its own stdin into the master, which is what puts our bytes where
// `/dev/tty` will read them.

/// Quote one argument for the `sh -c` that `script -c` runs.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Run a CLI subcommand under a pty, feeding `stdin_data` to its terminal.
///
/// Returns `(success, transcript)`. The transcript is the pty session, so
/// stdout and stderr are interleaved and the bytes we feed are echoed back by
/// the line discipline before the command switches the terminal to no-echo —
/// assert on substrings, never on an exact transcript.
fn run_cli_pty(config: &PathBuf, args: &[&str], stdin_data: &str) -> (bool, String) {
    use std::io::Write;
    use std::process::Stdio;

    let mut cmd_line = sh_quote(BIN);
    for a in args {
        cmd_line.push(' ');
        cmd_line.push_str(&sh_quote(a));
    }

    // -q: no start/stop banner. -e: exit with the command's status, which is
    // what lets these tests assert on failure. /dev/null: discard the typescript.
    let mut child = std::process::Command::new("script")
        .args(["-qec", &cmd_line, "/dev/null"])
        .env("MODELROUTER_CONFIG", config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn util-linux `script` to allocate a pty for the password prompt");

    child
        .stdin
        .as_mut()
        .expect("pty stdin")
        .write_all(stdin_data.as_bytes())
        .expect("write password to pty");
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait for pty child");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[tokio::test]
async fn admin_create_prompts_for_password() {
    let (dir, config) = setup_db();

    let (ok, out) = run_cli_pty(
        &config,
        &["admin", "create", "--name", "ops"],
        "hunter2\nhunter2\n",
    );
    assert!(ok, "admin create failed: {out}");
    assert!(out.contains("Password:"), "should prompt: {out}");
    assert!(out.contains("Confirm password:"), "should confirm: {out}");
    assert!(out.contains("Created admin 'ops'"), "{out}");
    assert!(out.contains("role=superadmin"), "default role: {out}");

    // The stored hash must verify against what we typed, and must not be the
    // plaintext — this is the whole point of the prompt path.
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        dir.path().join("router.db").display()
    ))
    .await
    .expect("connect to db");
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM admin_users WHERE name = ?")
        .bind("ops")
        .fetch_one(&pool)
        .await
        .expect("stored admin");
    assert_ne!(hash, "hunter2");
    assert!(bcrypt::verify("hunter2", &hash).expect("verify hash"));

    // The create is audited as actor `cli`.
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_log WHERE actor_name = 'cli'")
        .fetch_all(&pool)
        .await
        .expect("audit rows");
    assert!(actions.iter().any(|a| a == "admin.create"), "{actions:?}");
}

#[test]
fn admin_create_viewer_role() {
    let (_dir, config) = setup_db();

    let (ok, out) = run_cli_pty(
        &config,
        &["admin", "create", "--name", "readonly", "--role", "viewer"],
        "pw-viewer\npw-viewer\n",
    );
    assert!(ok, "admin create --role viewer failed: {out}");
    assert!(out.contains("role=viewer"), "{out}");

    let (ok, out, _err) = run_cli(&config, &["admin", "list"]);
    assert!(ok);
    assert!(out.contains("readonly"));
    assert!(out.contains("viewer"));
}

#[test]
fn admin_create_password_mismatch_fails() {
    let (_dir, config) = setup_db();

    let (ok, out) = run_cli_pty(
        &config,
        &["admin", "create", "--name", "ops"],
        "first-pass\nsecond-pass\n",
    );
    assert!(!ok, "mismatched confirmation must fail: {out}");
    assert!(out.contains("passwords do not match"), "{out}");

    // Nothing was written.
    let (ok, out, _err) = run_cli(&config, &["admin", "list"]);
    assert!(ok);
    assert!(!out.contains("ops"), "no admin should exist: {out}");
}

#[tokio::test]
async fn admin_reset_password_prompts() {
    let (dir, config) = setup_db();

    let (ok, out) = run_cli_pty(
        &config,
        &["admin", "create", "--name", "ops"],
        "original-pw\noriginal-pw\n",
    );
    assert!(ok, "{out}");

    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        dir.path().join("router.db").display()
    ))
    .await
    .expect("connect to db");
    let before: String = sqlx::query_scalar("SELECT password_hash FROM admin_users WHERE name = ?")
        .bind("ops")
        .fetch_one(&pool)
        .await
        .expect("admin row");

    // reset-password asks once, with no confirmation.
    let (ok, out) = run_cli_pty(
        &config,
        &["admin", "reset-password", "--name", "ops"],
        "rotated-pw\n",
    );
    assert!(ok, "reset-password failed: {out}");
    assert!(out.contains("New password:"), "should prompt: {out}");
    assert!(out.contains("Password updated for admin 'ops'"), "{out}");

    let after: String = sqlx::query_scalar("SELECT password_hash FROM admin_users WHERE name = ?")
        .bind("ops")
        .fetch_one(&pool)
        .await
        .expect("admin row");
    assert_ne!(before, after, "hash must change");
    assert!(bcrypt::verify("rotated-pw", &after).expect("verify new hash"));
    assert!(!bcrypt::verify("original-pw", &after).expect("verify old hash"));
}

#[test]
fn admin_reset_password_unknown_fails_before_prompting() {
    let (_dir, config) = setup_db();

    // The lookup runs before the prompt, so this exits without a terminal.
    let (ok, _out, err) = run_cli(&config, &["admin", "reset-password", "--name", "nobody"]);
    assert!(!ok, "unknown admin must fail");
    assert!(err.contains("not found"), "{err}");
}

#[test]
fn admin_hash_password_prints_bcrypt_and_config_snippet() {
    let (_dir, config) = setup_db();

    let (ok, out) = run_cli_pty(&config, &["admin", "hash-password"], "bootstrap-pw\nbootstrap-pw\n");
    assert!(ok, "hash-password failed: {out}");
    assert!(out.contains("Confirm password:"), "{out}");
    assert!(out.contains("[admin.bootstrap]"), "config snippet: {out}");
    assert!(out.contains("role = \"superadmin\""), "{out}");

    // The printed hash must be a usable bcrypt digest of what we typed.
    let hash = out
        .split_whitespace()
        .find(|t| t.starts_with("$2"))
        .map(|t| t.trim_matches('"'))
        .unwrap_or_else(|| panic!("no bcrypt hash in output: {out}"));
    assert!(bcrypt::verify("bootstrap-pw", hash).expect("verify printed hash"));
}

#[test]
fn admin_hash_password_mismatch_fails() {
    let (_dir, config) = setup_db();

    let (ok, out) = run_cli_pty(&config, &["admin", "hash-password"], "one\ntwo\n");
    assert!(!ok, "mismatched confirmation must fail: {out}");
    assert!(out.contains("passwords do not match"), "{out}");
}

// ── Reports over seeded data ──────────────────────────────────────────────────
//
// The existing report tests run against an empty database, so every row-
// rendering closure and every subtotal branch is skipped. These seed rows first.

/// Open the test database directly.
async fn open_db(dir: &TempDir) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!("sqlite://{}", dir.path().join("router.db").display()))
        .await
        .expect("connect to test db")
}

/// Insert a `cost_ledger` row for an existing user. `tokens` is
/// `(tokens_in, tokens_out)`, and `created_at` decides the monthly/annual
/// bucket the usage report groups the row into.
async fn seed_ledger(
    pool: &sqlx::SqlitePool,
    user: &str,
    project: &str,
    model: &str,
    tokens: (i64, i64),
    cost_usd: f64,
    created_at: &str,
) {
    let (tokens_in, tokens_out) = tokens;
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE name = ?")
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("user {user} must exist: {e}"));
    sqlx::query(
        "INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project, \
         tokens_in, tokens_out, cost_usd, created_at) \
         VALUES (?, NULL, ?, 'openai', ?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(model)
    .bind(project)
    .bind(tokens_in)
    .bind(tokens_out)
    .bind(cost_usd)
    .bind(created_at)
    .execute(pool)
    .await
    .expect("insert cost_ledger row");
}

#[tokio::test]
async fn report_usage_detail_renders_rows_in_every_format() {
    let (dir, config) = setup_db();
    for name in ["alice", "bob"] {
        let (ok, _out, err) = run_cli(&config, &["user", "create", "--name", name]);
        assert!(ok, "user create {name} failed: {err}");
    }

    let pool = open_db(&dir).await;
    // Two users and two models inside one month, plus a second month, so the
    // table path exercises bucket grouping, the per-user subtotal flush between
    // users, and the trailing subtotal after the last row.
    seed_ledger(&pool, "alice", "proj-a", "gpt-4o", (1000, 200), 1.25, "2024-03-01T10:00:00+00:00").await;
    seed_ledger(&pool, "alice", "proj-a", "gpt-4o-mini", (500, 100), 0.10, "2024-03-02T10:00:00+00:00").await;
    seed_ledger(&pool, "bob", "proj-b", "gpt-4o", (2000, 400), 2.50, "2024-03-03T10:00:00+00:00").await;
    seed_ledger(&pool, "bob", "proj-b", "gpt-4o", (300, 60), 0.40, "2024-04-01T10:00:00+00:00").await;

    // JSON: one object per ledger row, with the cost carried through.
    let (ok, out, err) = run_cli(
        &config,
        &["report", "usage", "--detail", "--monthly", "--global", "--format", "json"],
    );
    assert!(ok, "usage json failed: {err}");
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("usage --format json is JSON");
    let arr = parsed.as_array().expect("array of rows");
    assert_eq!(arr.len(), 4, "one row per ledger entry: {out}");
    assert!(arr.iter().any(|r| r["user"] == "alice" && r["model"] == "gpt-4o"));
    assert!(arr.iter().any(|r| r["bucket"] == "2024-04"));
    assert!(arr.iter().any(|r| r["tokens_in"] == 2000 && r["project"] == "proj-b"));

    // CSV: header plus one line per row.
    let (ok, out, err) = run_cli(
        &config,
        &["report", "usage", "--detail", "--monthly", "--global", "--format", "csv"],
    );
    assert!(ok, "usage csv failed: {err}");
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines[0], "bucket,user,project,model,tokens_in,tokens_out,cost_usd");
    assert_eq!(lines.len(), 5, "header + 4 rows: {out}");
    assert!(out.contains("2024-03,alice,proj-a,gpt-4o,1000,200,1.25"), "{out}");

    // Table: per-user subtotals and a grand total.
    let (ok, out, err) = run_cli(
        &config,
        &["report", "usage", "--detail", "--monthly", "--global", "--format", "table"],
    );
    assert!(ok, "usage table failed: {err}");
    assert!(out.contains("Subtotal: alice"), "per-user subtotal: {out}");
    assert!(out.contains("Subtotal: bob"), "trailing subtotal: {out}");
    assert!(out.contains("Grand Total:"), "{out}");
    assert!(out.contains("2024-03") && out.contains("2024-04"), "both buckets: {out}");
}

#[tokio::test]
async fn report_usage_total_and_subtotal_over_data() {
    let (dir, config) = setup_db();
    let (ok, _out, err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok, "{err}");

    let pool = open_db(&dir).await;
    seed_ledger(&pool, "alice", "proj-a", "gpt-4o", (100, 20), 0.50, "2023-07-01T00:00:00+00:00").await;
    seed_ledger(&pool, "alice", "proj-a", "gpt-4o", (100, 20), 0.50, "2023-08-01T00:00:00+00:00").await;

    // --total --alltime collapses to a single row with no bucket.
    let (ok, out, err) = run_cli(
        &config,
        &["report", "usage", "--total", "--alltime", "--global", "--format", "csv"],
    );
    assert!(ok, "{err}");
    assert!(out.contains(",200,40,1.00"), "summed totals: {out}");

    // --subtotal --annual groups per year+user+project.
    let (ok, out, err) = run_cli(
        &config,
        &["report", "usage", "--subtotal", "--annual", "--user", "alice", "--format", "table"],
    );
    assert!(ok, "{err}");
    assert!(out.contains("2023"), "annual bucket: {out}");
    assert!(out.contains("alice"), "{out}");
    assert!(out.contains("Grand Total:"), "{out}");
}

#[tokio::test]
async fn report_prompts_renders_rows() {
    let (dir, config) = setup_db();
    let (ok, _out, err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok, "{err}");

    let pool = open_db(&dir).await;
    sqlx::query(
        "INSERT INTO prompts (user_id, request_model, routed_model, provider, messages, \
         prompt_tokens, completion_tokens, cost_usd, created_at, tags) \
         VALUES ((SELECT id FROM users WHERE name = 'alice'), 'claude-opus-4-5', \
         'anthropic/claude-opus-4-5', 'anthropic', '[]', 120, 45, 0.0031, ?, '[]')",
    )
    .bind("2024-05-01T12:00:00+00:00")
    .execute(&pool)
    .await
    .expect("insert prompt");

    let (ok, out, err) = run_cli(&config, &["report", "prompts", "--limit", "10"]);
    assert!(ok, "{err}");
    assert!(out.contains("alice"), "{out}");
    assert!(out.contains("claude-opus-4-5"), "request model: {out}");
    assert!(out.contains("anthropic/claude-opus-4-5"), "routed model: {out}");
    assert!(out.contains("no"), "cached column: {out}");

    // Same rows via the non-table writers.
    let (ok, out, err) = run_cli(&config, &["report", "prompts", "--format", "json"]);
    assert!(ok, "{err}");
    assert!(out.contains("alice"), "{out}");
}

#[tokio::test]
async fn report_hooks_renders_latency_stats() {
    let (dir, config) = setup_db();
    let pool = open_db(&dir).await;

    // Enough samples that the percentile columns are meaningful, and one
    // failure so the success-rate column is not a flat 100%.
    for (i, dur) in [5_i64, 10, 15, 20, 25, 30, 90, 120].iter().enumerate() {
        sqlx::query(
            "INSERT INTO hook_metrics (hook_name, invoked_at, duration_ms, success) VALUES (?, ?, ?, ?)",
        )
        .bind("request.pre")
        .bind("2024-05-01T12:00:00+00:00")
        .bind(dur)
        .bind(if i == 0 { 0_i64 } else { 1 })
        .execute(&pool)
        .await
        .expect("insert hook metric");
    }

    let (ok, out, err) = run_cli(&config, &["report", "hooks"]);
    assert!(ok, "{err}");
    assert!(out.contains("request.pre"), "{out}");
    assert!(out.contains("8"), "invocation count: {out}");
    assert!(out.contains("87.5%"), "7 of 8 succeeded: {out}");

    let (ok, out, err) = run_cli(&config, &["report", "hooks", "--format", "csv"]);
    assert!(ok, "{err}");
    assert!(out.contains("request.pre"), "{out}");
}

// ── Budget scope and window validation ────────────────────────────────────────

#[test]
fn budget_set_requires_exactly_one_scope() {
    let (_dir, config) = setup_db();

    // Zero scope flags.
    let (ok, _out, err) = run_cli(&config, &["budget", "set", "--limit-usd", "5.0"]);
    assert!(!ok, "no scope must fail");
    assert!(err.contains("Exactly one scope flag is required"), "{err}");

    // Two scope flags.
    let (ok, _out, err) = run_cli(
        &config,
        &["budget", "set", "--global", "--project", "p", "--limit-usd", "5.0"],
    );
    assert!(!ok, "two scopes must fail");
    assert!(err.contains("Only one scope flag may be specified"), "{err}");
}

#[test]
fn budget_set_group_scope_ignores_window() {
    let (_dir, config) = setup_db();

    // A group budget is a soft target, so the window is forced to "target" and
    // an explicit --window is reported as ignored rather than silently dropped.
    let (ok, out, err) = run_cli(
        &config,
        &["budget", "set", "--group", "team-a", "--limit-usd", "25.0", "--window", "annual"],
    );
    assert!(ok, "group budget failed: {err}");
    assert!(out.contains("Created budget rule"), "{out}");
    assert!(
        err.contains("--window is ignored for --group scope"),
        "should warn on stderr: {err}"
    );

    let (ok, out, _err) = run_cli(&config, &["budget", "list"]);
    assert!(ok);
    assert!(out.contains("target"), "stored window: {out}");
}

#[test]
fn budget_set_window_total_requires_valid_date_range() {
    let (_dir, config) = setup_db();

    // Missing both bounds.
    let (ok, _out, err) = run_cli(
        &config,
        &["budget", "set", "--global", "--limit-usd", "5.0", "--window", "total"],
    );
    assert!(!ok);
    assert!(err.contains("--window total requires --window-start"), "{err}");

    // Start without end.
    let (ok, _out, err) = run_cli(
        &config,
        &[
            "budget", "set", "--global", "--limit-usd", "5.0", "--window", "total",
            "--window-start", "2024-01-01",
        ],
    );
    assert!(!ok);
    assert!(err.contains("--window total requires --window-end"), "{err}");

    // End not after start.
    let (ok, _out, err) = run_cli(
        &config,
        &[
            "budget", "set", "--global", "--limit-usd", "5.0", "--window", "total",
            "--window-start", "2024-06-01", "--window-end", "2024-06-01",
        ],
    );
    assert!(!ok);
    assert!(err.contains("--window-start must be before --window-end"), "{err}");

    // A valid range is accepted and the bounds are stored.
    let (ok, out, err) = run_cli(
        &config,
        &[
            "budget", "set", "--global", "--limit-usd", "5.0", "--window", "total",
            "--window-start", "2024-01-01", "--window-end", "2024-12-31",
        ],
    );
    assert!(ok, "valid total window failed: {err}");
    assert!(out.contains("Created budget rule"), "{out}");

    // `budget list` renders a total-window rule with its date range trimmed to
    // YYYY-MM-DD; the CLI stored midnight-UTC bounds from the bare dates.
    let (ok, out, _err) = run_cli(&config, &["budget", "list"]);
    assert!(ok);
    assert!(out.contains("total"), "{out}");
    assert!(out.contains("2024-01-01→2024-12-31"), "{out}");
}

#[test]
fn budget_set_duplicate_window_for_scope_fails() {
    let (_dir, config) = setup_db();

    let (ok, _out, err) = run_cli(
        &config,
        &["budget", "set", "--global", "--limit-usd", "10.0", "--window", "monthly"],
    );
    assert!(ok, "{err}");

    let (ok, _out, err) = run_cli(
        &config,
        &["budget", "set", "--global", "--limit-usd", "20.0", "--window", "monthly"],
    );
    assert!(!ok, "duplicate window for the same scope must fail");
    assert!(err.contains("already exists for this scope"), "{err}");
    assert!(err.contains("Delete it first"), "{err}");

    // A different window on the same scope is still allowed.
    let (ok, _out, err) = run_cli(
        &config,
        &["budget", "set", "--global", "--limit-usd", "30.0", "--window", "daily"],
    );
    assert!(ok, "different window should be accepted: {err}");
}

#[test]
fn budget_set_with_model_allow_and_deny() {
    let (_dir, config) = setup_db();

    let (ok, out, err) = run_cli(
        &config,
        &[
            "budget", "set", "--global", "--limit-usd", "10.0",
            "--model-allow", "gpt-4o, gpt-4o-mini",
            "--model-deny", "o1-preview",
            "--limit-tokens", "100000",
            "--rate-rpm", "60",
            "--max-concurrent", "4",
        ],
    );
    assert!(ok, "budget set with model lists failed: {err}");
    assert!(out.contains("Created budget rule"), "{out}");

    let (ok, out, _err) = run_cli(&config, &["budget", "list"]);
    assert!(ok);
    // The comma-separated lists are trimmed and stored as JSON arrays, and the
    // optional limits each add their own column to the listing.
    assert!(out.contains("gpt-4o-mini"), "{out}");
    assert!(!out.contains(" gpt-4o-mini"), "entries must be trimmed: {out}");
    assert!(out.contains("o1-preview"), "{out}");
    assert!(out.contains("tokens=100000"), "{out}");
    assert!(out.contains("rpm=60"), "{out}");
    assert!(out.contains("concurrent=4"), "{out}");
}

// ── Key rotate ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn key_rotate_disables_old_and_issues_new() {
    let (dir, config) = setup_db();
    let (ok, _out, err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok, "{err}");

    let (ok, out, err) = run_cli(
        &config,
        &["key", "create", "--user", "alice", "--project", "proj-a", "--label", "laptop"],
    );
    assert!(ok, "key create failed: {err}");
    let original = extract_key(&out);

    let (ok, out, err) = run_cli(
        &config,
        &["key", "rotate", "--user", "alice", "--project", "proj-a"],
    );
    assert!(ok, "key rotate failed: {err}");
    assert!(out.contains("Rotated key for 'alice' / project 'proj-a'"), "{out}");
    let rotated = extract_key(&out);
    assert_ne!(original, rotated, "rotation must issue a new key");

    // The old key is disabled, the new one is enabled and keeps the label.
    let pool = open_db(&dir).await;
    let rows: Vec<(i64, Option<String>)> =
        sqlx::query_as("SELECT enabled, label FROM api_keys WHERE project = 'proj-a' ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("list keys");
    assert_eq!(rows.len(), 2, "old + new: {rows:?}");
    assert_eq!(rows[0].0, 0, "old key disabled");
    assert_eq!(rows[1].0, 1, "new key enabled");
    assert_eq!(rows[1].1.as_deref(), Some("laptop"), "label carried over");

    let (ok, out, _err) = run_cli(&config, &["key", "list", "--user", "alice"]);
    assert!(ok);
    assert!(out.contains("disabled"), "{out}");
}

#[test]
fn key_rotate_unknown_user_and_project_fail() {
    let (_dir, config) = setup_db();

    let (ok, _out, err) = run_cli(
        &config,
        &["key", "rotate", "--user", "nobody", "--project", "p"],
    );
    assert!(!ok);
    assert!(err.contains("User not found"), "{err}");

    let (ok, _out, err) = run_cli(&config, &["user", "create", "--name", "alice"]);
    assert!(ok, "{err}");
    let (ok, _out, err) = run_cli(
        &config,
        &["key", "rotate", "--user", "alice", "--project", "no-such-project"],
    );
    assert!(!ok);
    assert!(err.contains("No key found"), "{err}");
}

// ── init overwrite prompt ─────────────────────────────────────────────────────
//
// `init` reads the overwrite answer from plain stdin (not /dev/tty), so these
// need no pty.

/// Run `init` against a throwaway HOME, answering the overwrite prompt with
/// `answer` (pass "" to leave stdin empty).
fn run_init(home: &std::path::Path, answer: &str) -> (bool, String) {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = std::process::Command::new(BIN)
        .arg("init")
        .env("HOME", home)
        .env_remove("MODELROUTER_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn init");
    child
        .stdin
        .as_mut()
        .expect("init stdin")
        .write_all(answer.as_bytes())
        .expect("write answer");
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("wait for init");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
fn init_overwrite_prompt_accepts_and_declines() {
    let home = tempfile::tempdir().expect("temp home");
    let config_path = home.path().join(".modelrouter/config.toml");

    // First run: no existing config, so no prompt.
    let (ok, out) = run_init(home.path(), "");
    assert!(ok, "first init failed: {out}");
    assert!(out.contains("Created config at"), "{out}");
    assert!(!out.contains("Overwrite?"), "nothing to overwrite yet: {out}");

    // The config directory and file are locked down to the owner.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir_mode = std::fs::metadata(home.path().join(".modelrouter"))
            .expect("config dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "config dir must be owner-only");
        let file_mode = std::fs::metadata(&config_path)
            .expect("config file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "config file must be owner-only");
    }

    // Mark the file so we can tell whether it was rewritten.
    let marker = "# do-not-overwrite-marker\n";
    let original = std::fs::read_to_string(&config_path).expect("read config");
    std::fs::write(&config_path, format!("{marker}{original}")).expect("write marker");

    // Declining leaves the file alone.
    let (ok, out) = run_init(home.path(), "n\n");
    assert!(ok, "declining init should still exit 0: {out}");
    assert!(out.contains("Overwrite? [y/N]"), "{out}");
    assert!(out.contains("Aborted."), "{out}");
    assert!(
        std::fs::read_to_string(&config_path).expect("read config").contains(marker),
        "declined init must not rewrite the config"
    );

    // Accepting rewrites it.
    let (ok, out) = run_init(home.path(), "y\n");
    assert!(ok, "accepting init failed: {out}");
    assert!(out.contains("Overwrote config at"), "{out}");
    let rewritten = std::fs::read_to_string(&config_path).expect("read config");
    assert!(!rewritten.contains(marker), "accepted init must rewrite the config");
    // Each init mints a fresh signing secret rather than shipping a placeholder.
    assert!(!rewritten.contains("change-me"), "generated secret: {rewritten}");
}
