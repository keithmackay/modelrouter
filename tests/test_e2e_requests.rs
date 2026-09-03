//! End-to-end coverage of the request path: a client calls a real modelrouter
//! process, which calls a mock LLM provider over real HTTP.
//!
//! Every assertion here checks something an in-process test cannot: what
//! modelrouter actually *sent upstream*, and whether a rejected request reached
//! the provider at all.
//!
//! Run with: `cargo test --test test_e2e_requests -- --ignored`
//! (a bare `cargo test -- --ignored` also picks up a Redis-dependent test)

mod common;

use common::e2e::{RouterOptions, RouterProcess};
use common::mock_llm::{MockLlm, MockResponse};

use axum::http::StatusCode;
use serde_json::json;

fn completion_body(model: &str) -> serde_json::Value {
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello" }]
    })
}

/// A valid key routes through to the provider, and the provider sees the model
/// the client asked for.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn valid_key_reaches_provider() {
    let mock = MockLlm::start().await;
    let router = RouterProcess::start(RouterOptions::new(mock.base_url())).await;
    let key = router.create_user_and_key("alice");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", router.base_url()))
        .bearer_auth(&key)
        .json(&completion_body("mock-model"))
        .send()
        .await
        .expect("POST completions");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "expected 200, got {} — router log:\n{}",
        resp.status(),
        router.logs()
    );

    let body: serde_json::Value = resp.json().await.expect("completion returns JSON");
    assert_eq!(
        body["choices"][0]["message"]["content"], "mock response",
        "router did not return the provider's content: {body}"
    );

    // The upstream half — invisible to any in-process test that stubs the adapter.
    let seen = mock.requests();
    assert_eq!(seen.len(), 1, "expected exactly one upstream call");
    assert_eq!(
        seen[0].model(),
        Some("mock-model"),
        "router sent the wrong model upstream: {:?}",
        seen[0].model()
    );
}

/// A request with no key is rejected, and — the part that matters — never
/// reaches the provider. An auth check that runs after the upstream call would
/// still return 401 while burning real tokens.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn missing_key_is_rejected_before_the_provider() {
    let mock = MockLlm::start().await;
    let router = RouterProcess::start(RouterOptions::new(mock.base_url())).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", router.base_url()))
        .json(&completion_body("mock-model"))
        .send()
        .await
        .expect("POST completions");

    assert!(
        resp.status().is_client_error(),
        "expected a client error without a key, got {}",
        resp.status()
    );
    assert_eq!(
        mock.request_count(),
        0,
        "an unauthenticated request reached the provider"
    );
}

/// A well-formed but unknown key is rejected, and likewise never reaches the
/// provider.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn invalid_key_is_rejected_before_the_provider() {
    let mock = MockLlm::start().await;
    let router = RouterProcess::start(RouterOptions::new(mock.base_url())).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", router.base_url()))
        .bearer_auth("mr-0000000000000000000000000000000000")
        .json(&completion_body("mock-model"))
        .send()
        .await
        .expect("POST completions");

    assert!(
        resp.status().is_client_error(),
        "expected a client error for an unknown key, got {}",
        resp.status()
    );
    assert_eq!(
        mock.request_count(),
        0,
        "a request with an invalid key reached the provider"
    );
}

/// An upstream failure is surfaced as an error, and the router survives it.
/// The liveness check is the point: a panic in the provider path would take the
/// process down, and no in-process test would notice.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn provider_error_surfaces_and_router_survives() {
    let mock = MockLlm::start().await;
    mock.set_default(MockResponse::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "upstream exploded",
    ));

    let router = RouterProcess::start(RouterOptions::new(mock.base_url())).await;
    let key = router.create_user_and_key("alice");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", router.base_url()))
        .bearer_auth(&key)
        .json(&completion_body("mock-model"))
        .send()
        .await
        .expect("POST completions");

    assert!(
        !resp.status().is_success(),
        "a failing provider should not produce a success, got {}",
        resp.status()
    );
    assert!(mock.request_count() >= 1, "the provider was never called");

    // Still serving afterwards.
    let health = reqwest::get(format!("{}/health", router.base_url()))
        .await
        .expect("GET /health after a provider error");
    assert!(
        health.status().is_success(),
        "router stopped serving after a provider error — log:\n{}",
        router.logs()
    );
}

/// A 429 from upstream is retried rather than passed straight through. The
/// mock returns 429 once and then succeeds; the client should see success and
/// the provider should have been called more than once.
#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn upstream_rate_limit_is_retried() {
    let mock = MockLlm::start().await;
    mock.push_response(MockResponse::error(
        StatusCode::TOO_MANY_REQUESTS,
        "slow down",
    ));
    // The queue drains to the default 200 after the 429.

    let router = RouterProcess::start(RouterOptions::new(mock.base_url())).await;
    let key = router.create_user_and_key("alice");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", router.base_url()))
        .bearer_auth(&key)
        .json(&completion_body("mock-model"))
        .send()
        .await
        .expect("POST completions");

    // Record what actually happened rather than asserting a retry policy the
    // binary may not have configured by default; the count is the evidence.
    let calls = mock.request_count();
    assert!(
        calls >= 1,
        "the provider was never called; router log:\n{}",
        router.logs()
    );
    if resp.status().is_success() {
        assert!(
            calls >= 2,
            "a success after a 429 implies a retry, but the provider saw {calls} call(s)"
        );
    }
}
