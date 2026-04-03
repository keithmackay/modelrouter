mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::Settings;
use modelrouter::db::models::NewUser;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine};
use std::collections::HashMap;
use std::sync::Arc;

async fn test_app() -> TestServer {
    let db = common::in_memory_db().await;

    // Create a test user
    db.create(NewUser {
        name: "test-user".to_string(),
        api_key_hash: hash_token("test-token"),
        group_name: None,
    })
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "Hello!".to_string(),
    }));

    let policy = Arc::new(PolicyEngine::new(db.clone()));

    let fallback = Arc::new(FallbackChain::new(HashMap::new()));

    let state = AppState {
        settings,
        db,
        pool: None,
        router,
        cost_calc,
        provider_registry,
        policy,
        fallback,
        app_metrics: None,
    };
    TestServer::new(build_router(state)).unwrap()
}

#[tokio::test]
async fn unauthenticated_request_returns_401() {
    let server = test_app().await;
    let resp = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn valid_request_returns_200() {
    let server = test_app().await;
    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["choices"][0]["message"]["content"], "Hello!");
}

#[test]
fn extract_text_from_sse_chunk_returns_delta_content() {
    let chunk = b"data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n";
    let result = modelrouter::api::routes::completions::extract_text_from_sse(chunk);
    assert_eq!(result, Some("Hello".to_string()));
}

#[test]
fn extract_text_from_done_returns_empty() {
    let chunk = b"data: [DONE]\n\n";
    let result = modelrouter::api::routes::completions::extract_text_from_sse(chunk);
    assert!(result.is_none());
}
