//! The PostgreSQL backend, end to end: the real binary serves against a live
//! PostgreSQL, the CLI writes to the same store, and a request's cost lands
//! in it. Everything else in the suite runs the backend per repository or
//! not at all; this is the test that proves `postgres_url` selects it.
//!
//! ```text
//! MODELROUTER_TEST_POSTGRES_URL=postgres://user:pass@localhost/modelrouter_test \
//!     cargo test --features postgres --test test_postgres_live -- --ignored
//! ```
#![cfg(feature = "postgres")]

mod common;

use common::e2e::{run_cli, without_ansi, RouterOptions, RouterProcess};
use common::mock_llm::MockLlm;
use std::time::{Duration, Instant};

fn pg_url() -> String {
    std::env::var("MODELROUTER_TEST_POSTGRES_URL")
        .expect("MODELROUTER_TEST_POSTGRES_URL must point at a scratch PostgreSQL database")
}

fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{}-{nanos}", std::process::id())
}

#[tokio::test]
#[ignore = "e2e: needs MODELROUTER_TEST_POSTGRES_URL"]
async fn serve_and_cli_run_against_postgres() {
    let mock = MockLlm::start().await;
    let router =
        RouterProcess::start(RouterOptions::new(mock.base_url()).with_postgres_url(pg_url())).await;
    let logs = without_ansi(&router.logs());
    assert!(
        logs.contains("backend=\"postgres\""),
        "serve must report the postgres backend:\n{logs}"
    );

    let health = reqwest::get(format!("{}/health", router.base_url()))
        .await
        .expect("GET /health");
    assert_eq!(health.status(), 200, "{}", router.logs());

    // The CLI opens the same store: user and key created there authenticate here.
    let user = unique("pguser");
    let key = router.create_user_and_key(&user);

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", router.base_url()))
        .header("Authorization", format!("Bearer {key}"))
        .json(&serde_json::json!({
            "model": "mock/test",
            "messages": [{"role": "user", "content": "hello"}],
        }))
        .send()
        .await
        .expect("chat request");
    assert!(
        resp.status().is_success(),
        "chat returned {}: {}",
        resp.status(),
        router.logs()
    );

    // Budget rules exercise the columns the parity migration adds.
    let (ok, _out, err) = run_cli(
        router.config_path(),
        &[
            "budget",
            "set",
            "--user",
            &user,
            "--limit-usd",
            "1.5",
            "--window",
            "monthly",
        ],
    );
    assert!(ok, "budget set on postgres: {err}");
    let (ok, out, err) = run_cli(router.config_path(), &["budget", "list"]);
    assert!(ok, "budget list on postgres: {err}");
    assert!(out.contains(&user), "{out}");

    // Reports still query SQLite directly and must say so instead of failing oddly.
    let (ok, _out, err) = run_cli(router.config_path(), &["report", "cost"]);
    assert!(!ok);
    assert!(
        err.contains("not available on the postgres backend"),
        "{err}"
    );

    // The request's cost row reached PostgreSQL.
    let pool = sqlx::PgPool::connect(&pg_url()).await.expect("connect");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM cost_ledger c JOIN users u ON u.id = c.user_id WHERE u.name = $1",
        )
        .bind(&user)
        .fetch_one(&pool)
        .await
        .expect("count ledger rows");
        if n >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no cost_ledger row for {user}:\n{}",
            router.logs()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
