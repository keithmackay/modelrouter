//! The `/v1/search` test app: a full router over an in-memory database, one
//! user with the bearer token `test-token`, and the given search registry and
//! request extensions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum_test::TestServer;
use modelrouter::api::app::{build_router_with_extensions, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::{CacheConfig, PricingEntry, Settings};
use modelrouter::db::models::{NewApiKey, NewUser};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::extensions::Extensions;
use modelrouter::providers::{
    embed_registry::EmbeddingRegistry, registry::ProviderRegistry,
    search::{SearchAdapter, SearchRequest, SearchResponse, SearchResultItem},
    search_registry::SearchRegistry,
};
use modelrouter::router::{
    cache::ResponseCache, complexity::ComplexityRouter, cost::CostCalculator,
    engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine,
};

pub async fn search_app(
    pricing: Vec<PricingEntry>,
    cache: CacheConfig,
    search_registry: SearchRegistry,
    default_search_engine: Option<&str>,
    extensions: Extensions,
) -> (TestServer, Arc<dyn DatabaseProvider>) {
    search_app_with(pricing, cache, search_registry, default_search_engine, extensions, |_| {}).await
}

/// [`search_app`], with a last chance to adjust the state (metrics, say).
pub async fn search_app_with(
    pricing: Vec<PricingEntry>,
    cache: CacheConfig,
    search_registry: SearchRegistry,
    default_search_engine: Option<&str>,
    extensions: Extensions,
    customize: impl FnOnce(&mut AppState),
) -> (TestServer, Arc<dyn DatabaseProvider>) {
    let db = super::in_memory_db().await;
    UserRepository::create(
        &db,
        NewUser {
            name: "test-user".to_string(),
            email: None,
        },
    )
    .await
    .unwrap();

    let user = UserRepository::find_by_name(&db, "test-user")
        .await
        .unwrap()
        .unwrap();
    ApiKeyRepository::create_api_key(
        &db,
        NewApiKey {
            user_id: user.id,
            key_hash: hash_token("test-token"),
            label: Some("test".to_string()),
            expires_at: None,
            project: None,
            session_window_secs: None,
        },
    )
    .await
    .unwrap();

    let mut settings = Settings::default();
    settings.pricing = pricing;
    settings.routing.default_search_engine = default_search_engine.map(str::to_string);
    let settings = Arc::new(settings);
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    // As in production: the calculator carries the configured [[pricing]].
    let cost_calc = Arc::new(CostCalculator::new_with_config(&settings.pricing));
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(super::MockAdapter {
        response: "hello".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(ComplexityRouter::new(None));
    let response_cache = Arc::new(ResponseCache::new(&cache));
    let embedding_registry = Arc::new(EmbeddingRegistry::new_with_mock(
        super::MockEmbeddingAdapter {
            embedding: vec![0.1_f32, 0.2, 0.3],
        },
    ));
    let search_registry = Arc::new(search_registry);

    let mut state = AppState {
        settings: settings.clone(),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
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
        search_registry,
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(
            modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0),
        ),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(
            0, 0,
        )),
        session_affinity: Arc::new(
            modelrouter::router::session_affinity::SessionAffinityMap::new(1800),
        ),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
    };
    customize(&mut state);
    (TestServer::new(build_router_with_extensions(state, extensions)).unwrap(), db)
}

/// A search adapter that records every request it is sent.
#[derive(Clone, Default)]
pub struct RecordingSearchAdapter {
    pub requests: Arc<Mutex<Vec<SearchRequest>>>,
}

#[async_trait::async_trait]
impl SearchAdapter for RecordingSearchAdapter {
    async fn search(&self, req: &SearchRequest) -> anyhow::Result<SearchResponse> {
        self.requests.lock().unwrap().push(req.clone());
        Ok(SearchResponse {
            results: vec![SearchResultItem {
                title: "Example Domain".to_string(),
                url: "https://example.com".to_string(),
                snippet: "Example description".to_string(),
                score: Some(0.9),
                published_date: None,
            }],
            engine: "tavily".to_string(),
            answer: None,
        })
    }
}
