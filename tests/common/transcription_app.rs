//! A router app on a loopback port with one streaming transcription registry.
//! Shared by the route tests (fake provider session) and the Foundry adapter
//! tests (mock realtime server). The WebSocket client for its route lives in
//! `ws_client.rs`, included by those tests directly: this module is compiled
//! by every crate that reuses `tests/common`, and not all of them carry the
//! WebSocket client crate.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use modelrouter::api::app::{build_router_with_transcription_registry, AppState, DatabaseProvider};
use modelrouter::config::schema::{CacheConfig, ProviderConfig, Settings, TranscriptionConfig};
use modelrouter::db::models::NewBudgetRule;
use modelrouter::db::repositories::budgets::BudgetRepository;
use modelrouter::extensions::Extensions;
use modelrouter::providers::transcription_registry::TranscriptionRegistry;
use modelrouter::providers::{embed_registry::EmbeddingRegistry, registry::ProviderRegistry};
use modelrouter::router::experiments::ExperimentRegistry as LiveExperiments;
use modelrouter::router::{
    cache::ResponseCache, complexity::ComplexityRouter, cost::CostCalculator, engine::RequestRouter,
    fallback::FallbackChain, policy::PolicyEngine,
};

pub const TOKEN: &str = "test-token";

pub struct TranscriptionApp {
    pub base_url: String,
    pub db: Arc<dyn DatabaseProvider>,
}

pub struct AppOptions {
    pub transcription: TranscriptionConfig,
    pub providers: HashMap<String, ProviderConfig>,
    /// Deny this model for the test user through a budget rule.
    pub deny_model: Option<String>,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self { transcription: TranscriptionConfig::default(), providers: HashMap::new(), deny_model: None }
    }
}

/// Build the app around `registry` and serve it. `registry` is built by the
/// caller from `opts.transcription` so a test can pin an adapter into it.
pub async fn serve(opts: AppOptions, build_registry: impl FnOnce(TranscriptionRegistry) -> TranscriptionRegistry) -> TranscriptionApp {
    let db = super::in_memory_db().await;
    let user_id = super::create_user(&db, "test-user", TOKEN).await;
    if let Some(model) = &opts.deny_model {
        BudgetRepository::create(
            &db as &dyn DatabaseProvider,
            NewBudgetRule {
                user_id: Some(user_id),
                group_name: None,
                api_key_id: None,
                tag: None,
                project: None,
                window: "monthly".to_string(),
                limit_usd: None,
                limit_tokens: None,
                rate_rpm: None,
                max_concurrent: None,
                model_allow: vec![],
                model_deny: vec![model.clone()],
                window_start: None,
                window_end: None,
            },
        )
        .await
        .unwrap();
    }

    let mut settings = Settings::default();
    settings.providers = opts.providers.clone();
    settings.transcription = opts.transcription.clone();
    let settings = Arc::new(settings);
    let registry = Arc::new(build_registry(TranscriptionRegistry::new(opts.transcription, opts.providers)));

    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new_with_config(&[])),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(super::MockAdapter { response: "hello".to_string() })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache: Arc::new(ResponseCache::new(&CacheConfig::default())),
        embedding_registry: Arc::new(EmbeddingRegistry::new_with_mock(super::MockEmbeddingAdapter { embedding: vec![0.1] })),
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(HashMap::new())),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(HashMap::new())),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(LiveExperiments::default()),
    };
    let app = build_router_with_transcription_registry(state, Extensions::default(), registry);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    TranscriptionApp { base_url: format!("http://{addr}"), db }
}

/// The default `session.start` for a 16 kHz mono caller.
pub fn start_message(session_id: &str, model: Option<&str>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "type": "session.start",
        "session_id": session_id,
        "audio": { "encoding": "pcm16", "sample_rate_hz": 16000, "channels": 1 },
        "language": "en",
    });
    if let Some(model) = model {
        v["model"] = serde_json::json!(model);
    }
    v
}
