//! End-to-end coverage of the startup path: `init`, `migrate`, and `serve`.
//!
//! This is the tier that did not exist when a startup guard was added that
//! refused to boot on the placeholder secret `init` itself writes. Every one of
//! the 425 in-process tests passed, because none of them executes `serve`.
//!
//! Run with: `cargo test --test test_e2e_startup -- --ignored`

mod common;

use common::e2e::{run_cli, try_serve_expecting_exit, RouterOptions, RouterProcess};
use common::mock_llm::MockLlm;

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_modelrouter");

/// The regression test for the `init` placeholder-secret defect.
///
/// `init` writes a config and prints "Run: modelrouter serve". This asserts
/// that sequence actually works — that the quickstart an operator is handed
/// does not lead into a refusal. Reverting the fix that makes `init` generate a
/// real secret fails here.
///
/// `init` ignores `--config` and always writes to the home directory, so this
/// overrides `HOME` rather than touching the developer's real install.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn init_then_serve_starts() {
    let home = tempfile::tempdir().expect("temp home");
    let out = Command::new(BIN)
        .arg("init")
        .env("HOME", home.path())
        .output()
        .expect("run init");
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let config = home.path().join(".modelrouter/config.toml");
    assert!(config.exists(), "init did not write a config at {config:?}");

    let written = std::fs::read_to_string(&config).expect("read generated config");
    assert!(
        !written.contains("change-me-jwt-secret"),
        "init wrote the shipped placeholder secret; `serve` will refuse to start"
    );

    // Serve the config `init` produced. This is the assertion that matters:
    // the quickstart `init` prints must actually reach a serving process.
    // Before the fix this refused to start on the placeholder secret.
    let router = RouterProcess::start_with_config(config, home).await;
    let resp = reqwest::get(format!("{}/health", router.base_url()))
        .await
        .expect("GET /health after init");
    assert!(
        resp.status().is_success(),
        "serving the config init generated returned {}",
        resp.status()
    );
}

/// The guard itself: a config carrying the shipped placeholder must refuse,
/// and must name the field so the operator knows what to fix.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn placeholder_secret_refuses_to_start() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        common::e2e::render_config(
            dir.path(),
            18_999,
            &RouterOptions::new("http://127.0.0.1:1").with_jwt_secret("change-me-jwt-secret"),
        ),
    )
    .expect("write config");

    let (_ok, output) = try_serve_expecting_exit(&config);
    assert!(
        output.contains("jwt_secret"),
        "expected a refusal naming auth.jwt_secret, got:\n{output}"
    );
    assert!(
        output.contains("placeholder"),
        "the refusal should say the value is the shipped placeholder, got:\n{output}"
    );
}

/// The empty-secret arm of the same guard. This is the value the Helm chart
/// shipped, so it is the case that actually occurred in production.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn empty_secret_refuses_to_start() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        common::e2e::render_config(
            dir.path(),
            18_998,
            &RouterOptions::new("http://127.0.0.1:1").with_jwt_secret(""),
        ),
    )
    .expect("write config");

    let (_ok, output) = try_serve_expecting_exit(&config);
    assert!(
        output.contains("jwt_secret") && output.contains("empty"),
        "expected a refusal naming an empty auth.jwt_secret, got:\n{output}"
    );
}

/// `migrate` against a fresh path creates the database and its schema.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn migrations_apply_to_a_fresh_database() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        common::e2e::render_config(dir.path(), 18_997, &RouterOptions::new("http://127.0.0.1:1")),
    )
    .expect("write config");

    let (ok, _out, err) = run_cli(&config, &["migrate"]);
    assert!(ok, "migrate failed: {err}");

    let db = dir.path().join("router.db");
    assert!(db.exists(), "migrate did not create {db:?}");

    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db.display()))
        .await
        .expect("open migrated database");
    for table in ["users", "api_keys", "cost_ledger", "prompts"] {
        let found: Option<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' AND name = ?")
                .bind(table)
                .fetch_optional(&pool)
                .await
                .expect("query sqlite_master");
        assert!(found.is_some(), "expected table `{table}` after migrate");
    }
}

/// Running `migrate` twice must succeed. A deployment that restarts re-runs it.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn migrations_are_idempotent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        common::e2e::render_config(dir.path(), 18_996, &RouterOptions::new("http://127.0.0.1:1")),
    )
    .expect("write config");

    let (first, _o, e1) = run_cli(&config, &["migrate"]);
    assert!(first, "first migrate failed: {e1}");
    let (second, _o, e2) = run_cli(&config, &["migrate"]);
    assert!(second, "second migrate failed, migrations are not idempotent: {e2}");
}

/// The fixture itself works: a real process starts, answers `/health`, and is
/// reachable over a real socket. Everything else in this tier depends on it.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn fixture_starts_a_healthy_server() {
    let mock = MockLlm::start().await;
    let router = RouterProcess::start(RouterOptions::new(mock.base_url())).await;

    let resp = reqwest::get(format!("{}/health", router.base_url()))
        .await
        .expect("GET /health");
    assert!(resp.status().is_success(), "health returned {}", resp.status());

    let body: serde_json::Value = resp.json().await.expect("health returns JSON");
    assert_eq!(body["status"], "ok", "unexpected health body: {body}");
}
