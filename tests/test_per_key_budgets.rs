mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::Settings;
use modelrouter::db::models::{NewUser, NewApiKey};
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{
    cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain,
    policy::PolicyEngine, complexity::ComplexityRouter,
};
use std::collections::HashMap;
use std::sync::Arc;

async fn test_app() -> (TestServer, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;

    db.create(NewUser {
        name: "base-user".to_string(),
        api_key_hash: hash_token("legacy-token"),
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
    let complexity_router = Arc::new(ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default()
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        )
    );
    let load_balancer = Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
        std::collections::HashMap::new(),
    ));

    let state = AppState {
        settings,
        db: db.clone(),
        pool: None,
        router,
        cost_calc,
        provider_registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        load_balancer,
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        app_metrics: None,
    };
    (TestServer::new(build_router(state)).unwrap(), db)
}

#[tokio::test]
async fn api_key_auth_works() {
    let (server, db) = test_app().await;

    // Create an API key for the base user
    let user = UserRepository::find_by_name(&*db, "base-user").await.unwrap().unwrap();
    ApiKeyRepository::create_api_key(&*db, NewApiKey {
        user_id: user.id,
        key_hash: hash_token("per-key-token"),
        label: Some("test-key".to_string()),
        expires_at: None,
    })
    .await
    .unwrap();

    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer per-key-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn legacy_token_still_works() {
    let (server, _db) = test_app().await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer legacy-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn revoked_api_key_returns_401() {
    let (server, db) = test_app().await;
    let user = UserRepository::find_by_name(&*db, "base-user").await.unwrap().unwrap();

    let key = ApiKeyRepository::create_api_key(&*db, NewApiKey {
        user_id: user.id,
        key_hash: hash_token("revokable-token"),
        label: None,
        expires_at: None,
    })
    .await
    .unwrap();

    ApiKeyRepository::revoke_api_key(&*db, key.id).await.unwrap();

    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer revokable-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hi"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 401);
}
