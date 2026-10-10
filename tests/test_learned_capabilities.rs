//! Learning a model's `temperature` rejection from the provider, end to end:
//! a fake provider rejects `temperature` for one model snapshot, the router
//! retries without it, persists what it learned, and never sends it again —
//! including after a restart on the same database.

mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::ModelCapabilityEntry;
use modelrouter::config::Settings;
use modelrouter::db::models::{NewApiKey, NewUser};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::learned_capabilities::LearnedCapabilityRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::db::sqlite::SqliteDb;
use modelrouter::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const REJECTING_MODEL: &str = "model-x@v1";
const UNRELATED_400_MODEL: &str = "model-y@v1";

/// Records each call's model and temperature. Rejects `temperature` for
/// `REJECTING_MODEL`, and answers every call to `UNRELATED_400_MODEL` with a
/// 400 that has nothing to do with temperature.
#[derive(Clone, Default)]
struct FakeProvider {
    calls: Arc<Mutex<Vec<(String, Option<f64>)>>>,
}

impl FakeProvider {
    fn respond(&self, req: &NormalizedRequest) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push((req.model.clone(), req.temperature));
        if req.model.ends_with(REJECTING_MODEL) && req.temperature.is_some() {
            anyhow::bail!(
                "Fake returned 400 Bad Request: {{\"type\":\"error\",\"error\":{{\"type\":\"invalid_request_error\",\"message\":\"`temperature` is deprecated for this model.\"}}}}"
            );
        }
        if req.model.ends_with(UNRELATED_400_MODEL) {
            anyhow::bail!("Fake returned 400 Bad Request: max_tokens: must be at most 1024");
        }
        Ok(())
    }

    fn take_calls(&self) -> Vec<(String, Option<f64>)> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for FakeProvider {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        self.respond(req)?;
        Ok(CompletionResult {
            content: "ok".to_string(),
            prompt_tokens: 1,
            completion_tokens: 1,
            finish_reason: "stop".to_string(),
            ..Default::default()
        })
    }

    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        self.respond(req)?;
        let data = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
        Ok(Box::pin(futures::stream::once(async move {
            Ok::<bytes::Bytes, anyhow::Error>(bytes::Bytes::from(data))
        })))
    }
}

async fn seed_user(db: &SqliteDb) {
    if UserRepository::find_by_name(db, "test-user").await.unwrap().is_some() {
        return;
    }
    db.create(NewUser { name: "test-user".to_string(), email: None }).await.unwrap();
    let user = UserRepository::find_by_name(db, "test-user").await.unwrap().unwrap();
    ApiKeyRepository::create_api_key(db, NewApiKey {
        user_id: user.id,
        key_hash: hash_token("test-token"),
        label: Some("test".to_string()),
        expires_at: None,
        project: None,
        session_window_secs: None,
    })
    .await
    .unwrap();
}

/// One router process over `db`: loads the stored learned capabilities the
/// way `serve` does at startup.
async fn start(
    db: Arc<dyn DatabaseProvider>,
    provider: FakeProvider,
    overrides: Vec<ModelCapabilityEntry>,
) -> (TestServer, Arc<RequestRouter>) {
    let settings = Arc::new(Settings { model_capabilities: overrides, ..Settings::default() });
    let router = Arc::new(RequestRouter::new(settings.clone()));
    modelrouter::api::admin::learned_capabilities::load_learned_capabilities(&router, &*db).await;
    let state = AppState {
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        settings,
        db: db.clone(),
        pool: None,
        router: router.clone(),
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(provider)),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
        response_cache: Arc::new(modelrouter::router::cache::ResponseCache::new(
            &modelrouter::config::schema::CacheConfig::default(),
        )),
        embedding_registry: Arc::new(modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32] },
        )),
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(HashMap::new())),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(HashMap::new())),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    (TestServer::new(build_router(state)).unwrap(), router)
}

async fn chat(server: &TestServer, model: &str, stream: bool) -> u16 {
    server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": format!("fake/{model}"),
            "temperature": 0.3,
            "stream": stream,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await
        .status_code()
        .as_u16()
}

async fn file_db(dir: &tempfile::TempDir) -> SqliteDb {
    let path = dir.path().join("router.db");
    let db = SqliteDb::connect(path.to_str().unwrap()).await.unwrap();
    modelrouter::db::migrations::run_migrations(&db.pool).await.unwrap();
    seed_user(&db).await;
    db
}

#[tokio::test]
async fn rejection_is_retried_learned_persisted_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let provider = FakeProvider::default();
    let db: Arc<dyn DatabaseProvider> = Arc::new(file_db(&dir).await);
    let (server, router) = start(db.clone(), provider.clone(), vec![]).await;

    // First call: rejected with temperature, retried without, succeeds.
    assert_eq!(chat(&server, REJECTING_MODEL, false).await, 200);
    assert_eq!(
        provider.take_calls(),
        vec![
            (REJECTING_MODEL.to_string(), Some(0.3)),
            (REJECTING_MODEL.to_string(), None),
        ]
    );

    // Second call: temperature never sent, one provider call.
    assert_eq!(chat(&server, REJECTING_MODEL, false).await, 200);
    assert_eq!(provider.take_calls(), vec![(REJECTING_MODEL.to_string(), None)]);

    // Streaming takes the same path.
    assert_eq!(chat(&server, REJECTING_MODEL, true).await, 200);
    assert_eq!(provider.take_calls(), vec![(REJECTING_MODEL.to_string(), None)]);

    // Stored, with the error that taught it, and counted.
    let stored = db.list_learned_capabilities().await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].model, REJECTING_MODEL);
    assert!(!stored[0].supports_temperature);
    assert!(stored[0].error.contains("`temperature` is deprecated"));
    let views = router.learned_capabilities().snapshot();
    assert_eq!(views[0].stripped_since_start, 2);

    // Another snapshot of the same model still receives temperature.
    assert_eq!(chat(&server, "model-x@v2", false).await, 200);
    assert_eq!(provider.take_calls(), vec![("model-x@v2".to_string(), Some(0.3))]);

    // Restart on the same database: the first call already omits temperature.
    drop(server);
    let db: Arc<dyn DatabaseProvider> = Arc::new(file_db(&dir).await);
    let (server, _router) = start(db, provider.clone(), vec![]).await;
    assert_eq!(chat(&server, REJECTING_MODEL, false).await, 200);
    assert_eq!(provider.take_calls(), vec![(REJECTING_MODEL.to_string(), None)]);
}

#[tokio::test]
async fn config_override_beats_the_learned_entry() {
    let dir = tempfile::tempdir().unwrap();
    let provider = FakeProvider::default();
    let db: Arc<dyn DatabaseProvider> = Arc::new(file_db(&dir).await);
    let overrides = vec![ModelCapabilityEntry {
        model: REJECTING_MODEL.to_string(),
        supports_temperature: Some(true),
        ..Default::default()
    }];
    let (server, _router) = start(db.clone(), provider.clone(), overrides).await;

    // The call still succeeds via the retry, and the rejection is recorded...
    assert_eq!(chat(&server, REJECTING_MODEL, false).await, 200);
    assert_eq!(provider.take_calls().len(), 2);
    assert_eq!(db.list_learned_capabilities().await.unwrap().len(), 1);
    // ...but the operator's explicit entry decides what is sent.
    assert_eq!(chat(&server, REJECTING_MODEL, false).await, 200);
    assert_eq!(provider.take_calls()[0], (REJECTING_MODEL.to_string(), Some(0.3)));
}

#[tokio::test]
async fn an_unrelated_400_learns_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let provider = FakeProvider::default();
    let db: Arc<dyn DatabaseProvider> = Arc::new(file_db(&dir).await);
    let (server, router) = start(db.clone(), provider.clone(), vec![]).await;

    assert_ne!(chat(&server, UNRELATED_400_MODEL, false).await, 200);
    let calls = provider.take_calls();
    assert!(calls.iter().all(|(_, t)| *t == Some(0.3)), "never retried without temperature: {calls:?}");
    assert!(db.list_learned_capabilities().await.unwrap().is_empty());
    assert!(router.learned_capabilities().snapshot().is_empty());
}
