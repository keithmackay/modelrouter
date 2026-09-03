//! End-to-end coverage of the side effects a request leaves behind: the cost
//! ledger, the response cache, and the streaming path.
//!
//! These assert against the real SQLite file the running process writes, not
//! through an admin endpoint. The point is to prove the row exists, not that a
//! page renders.
//!
//! Run with: `cargo test --test test_e2e_accounting -- --ignored`

mod common;

use common::e2e::{RouterOptions, RouterProcess};
use common::mock_llm::{MockLlm, MockResponse};

use serde_json::json;
use sqlx::Row;
use std::time::{Duration, Instant};

/// Cost logging is fire-and-forget (`tokio::spawn` after the response), so the
/// row appears shortly *after* the client sees its answer. Poll rather than
/// sleep a fixed duration.
async fn wait_for_ledger_rows(db: &std::path::Path, want: i64) -> i64 {
    let url = format!("sqlite://{}", db.display());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = -1;

    while Instant::now() < deadline {
        if let Ok(pool) = sqlx::SqlitePool::connect(&url).await {
            if let Ok(row) = sqlx::query("SELECT COUNT(*) AS n FROM cost_ledger")
                .fetch_one(&pool)
                .await
            {
                last = row.get::<i64, _>("n");
                if last >= want {
                    pool.close().await;
                    return last;
                }
            }
            pool.close().await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    last
}

fn completion_body(model: &str) -> serde_json::Value {
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello" }],
        // Explicit temperature 0 — the cache policy's default max_temperature is
        // 0.0 and an omitted value is scored as 1.0, so omitting it means the
        // request is never cache-eligible.
        "temperature": 0
    })
}

/// Token usage the provider reports becomes a ledger row carrying the model and
/// provider that served it.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn usage_becomes_a_ledger_row() {
    let mock = MockLlm::start().await;
    mock.set_default(MockResponse::completion("counted", 123, 45));

    let router = RouterProcess::start(RouterOptions::new(mock.base_url())).await;
    let key = router.create_user_and_key("alice");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", router.base_url()))
        .bearer_auth(&key)
        .json(&completion_body("mock-model"))
        .send()
        .await
        .expect("POST completions");
    assert!(resp.status().is_success(), "request failed: {}", resp.status());

    let count = wait_for_ledger_rows(&router.db_path(), 1).await;
    assert_eq!(
        count, 1,
        "expected one cost_ledger row, found {count} — router log:\n{}",
        router.logs()
    );

    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", router.db_path().display()))
        .await
        .expect("open router database");
    let row = sqlx::query("SELECT model, provider, tokens_in, tokens_out FROM cost_ledger LIMIT 1")
        .fetch_one(&pool)
        .await
        .expect("read the ledger row");

    assert_eq!(row.get::<String, _>("provider"), "mock", "wrong provider recorded");
    assert_eq!(row.get::<String, _>("model"), "mock-model", "wrong model recorded");
    assert_eq!(
        row.get::<i64, _>("tokens_in"),
        123,
        "prompt tokens were not taken from the provider's usage block"
    );
    assert_eq!(
        row.get::<i64, _>("tokens_out"),
        45,
        "completion tokens were not taken from the provider's usage block"
    );
}

/// With the cache enabled, an identical eligible request is served twice but
/// reaches the provider once. The upstream call count is the assertion — a
/// response header alone would not prove the call was avoided.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn identical_request_reaches_the_provider_once() {
    let mock = MockLlm::start().await;
    let router = RouterProcess::start(RouterOptions::new(mock.base_url()).with_cache()).await;
    let key = router.create_user_and_key("alice");

    let client = reqwest::Client::new();
    let url = format!("{}/v1/chat/completions", router.base_url());
    let body = completion_body("mock-model");

    let first = client
        .post(&url)
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
        .expect("first request");
    assert!(first.status().is_success(), "first request failed: {}", first.status());

    // The store happens after the response is returned; give it a moment to land
    // before asking the same question again.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let second = client
        .post(&url)
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
        .expect("second request");
    assert!(second.status().is_success(), "second request failed: {}", second.status());

    assert_eq!(
        mock.request_count(),
        1,
        "an identical cache-eligible request hit the provider twice; router log:\n{}",
        router.logs()
    );

    let cache_header = second
        .headers()
        .get("x-modelrouter-cache")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        cache_header, "HIT",
        "second response should be marked as a cache hit, got {cache_header:?}"
    );
}

/// With the cache disabled — the default — the same two requests both reach the
/// provider. This is the negative control for the test above: without it, a
/// cache that silently never engaged would look identical to one that works.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn without_the_cache_both_requests_reach_the_provider() {
    let mock = MockLlm::start().await;
    let router = RouterProcess::start(RouterOptions::new(mock.base_url())).await;
    let key = router.create_user_and_key("alice");

    let client = reqwest::Client::new();
    let url = format!("{}/v1/chat/completions", router.base_url());
    let body = completion_body("mock-model");

    for _ in 0..2 {
        let resp = client
            .post(&url)
            .bearer_auth(&key)
            .json(&body)
            .send()
            .await
            .expect("request");
        assert!(resp.status().is_success());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(
        mock.request_count(),
        2,
        "with the cache off both requests should reach the provider"
    );
}

/// A streaming request returns an event stream and leaves the process serving.
/// Deeper streaming assertions — incremental delivery, ledger write after the
/// stream ends — are deferred; this is the smoke check that the path works at all.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn streaming_request_returns_an_event_stream() {
    let mock = MockLlm::start().await;
    let router = RouterProcess::start(RouterOptions::new(mock.base_url())).await;
    let key = router.create_user_and_key("alice");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", router.base_url()))
        .bearer_auth(&key)
        .json(&json!({
            "model": "mock-model",
            "messages": [{ "role": "user", "content": "hello" }],
            "stream": true
        }))
        .send()
        .await
        .expect("POST streaming completion");

    assert!(
        resp.status().is_success(),
        "streaming request failed: {} — router log:\n{}",
        resp.status(),
        router.logs()
    );

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.text().await.expect("read stream body");

    assert!(
        content_type.contains("event-stream") || body.contains("data:"),
        "expected an SSE response, got content-type {content_type:?} and body:\n{body}"
    );

    let seen = mock.requests();
    assert_eq!(seen.len(), 1, "expected one upstream call");
    assert!(
        seen[0].is_stream(),
        "router did not forward stream=true upstream"
    );

    let health = reqwest::get(format!("{}/health", router.base_url()))
        .await
        .expect("GET /health after streaming");
    assert!(health.status().is_success(), "router stopped serving after a stream");
}
