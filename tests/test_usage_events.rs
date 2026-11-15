//! Every request-path cost-ledger row emits exactly one `usage` callback event
//! carrying the request's attribution: streaming and non-streaming chat, the
//! Messages API, response-cache hits, /v1/responses, embeddings, search,
//! images and audio. Rerank and decisions are covered in their own suites.
//! The health probe's synthetic rows emit none.

mod common;

use axum_test::TestServer;
use common::RecordedUsage;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::{CacheConfig, PricingEntry, ProviderConfig, Settings};
use modelrouter::providers::{
    embed_registry::EmbeddingRegistry, registry::ProviderRegistry, search::SearchResultItem,
    search_registry::SearchRegistry,
};
use modelrouter::router::{
    cache::ResponseCache, complexity::ComplexityRouter, cost::CostCalculator,
    engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

struct Harness {
    server: TestServer,
    db: Arc<dyn DatabaseProvider>,
    usage: RecordedUsage,
    _anthropic: common::mock_anthropic::MockAnthropicServer,
    _openai: common::mock_audio::MockAudioServer,
}

async fn harness(cache: CacheConfig) -> Harness {
    let anthropic = common::mock_anthropic::MockAnthropicServer::start().await;
    let openai = common::mock_audio::MockAudioServer::start().await;
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;

    let mut settings = Settings::default();
    settings.providers.insert(
        "anthropic".to_string(),
        ProviderConfig {
            api_key: "test-key".to_string(),
            api_base: Some(anthropic.base_url()),
            timeout_secs: 10,
            ..Default::default()
        },
    );
    settings.providers.insert(
        "openai".to_string(),
        ProviderConfig {
            api_key: "test-key".to_string(),
            api_base: Some(openai.base_url()),
            timeout_secs: 10,
            ..Default::default()
        },
    );
    settings.pricing = vec![PricingEntry {
        model: "mock-model".to_string(),
        input_per_million: 1000.0,
        output_per_million: 1000.0,
        ..Default::default()
    }];
    settings.health.embedding_probe_model = "openai/text-embedding-3-small".to_string();
    let settings = Arc::new(settings);
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let usage = RecordedUsage::default();

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new_with_config(&settings.pricing)),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
            response: "hello".to_string(),
        })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache: Arc::new(ResponseCache::new(&cache)),
        embedding_registry: Arc::new(EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        )),
        search_registry: Arc::new(SearchRegistry::new_with_mock(common::MockSearchAdapter {
            results: vec![SearchResultItem {
                title: "Example".to_string(),
                url: "https://example.com".to_string(),
                snippet: "Example".to_string(),
                score: Some(0.9),
                published_date: None,
                publisher: None,
                metadata_provenance: None,
            }],
        })),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(
            modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0),
        ),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(
            modelrouter::router::session_affinity::SessionAffinityMap::new(1800),
        ),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: usage.dispatcher(),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    Harness {
        server: TestServer::new(build_router(state)).unwrap(),
        db,
        usage,
        _anthropic: anthropic,
        _openai: openai,
    }
}

fn caching() -> CacheConfig {
    CacheConfig {
        enabled: true,
        max_entries: 32,
        ttl_seconds: 300,
        ..Default::default()
    }
}

/// POST `body` to `path` as the test user, tagged with the attribution
/// headers every case checks for.
async fn tagged_post(h: &Harness, path: &str, body: Value) -> axum_test::TestResponse {
    tagged(h.server.post(path)).json(&body).await
}

/// `request` as the test user, tagged with the attribution headers every case
/// checks for.
fn tagged(request: axum_test::TestRequest) -> axum_test::TestRequest {
    let header = |name: &'static str| axum::http::HeaderName::from_static(name);
    request
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .add_header(
            header("x-attribution-correlation-id"),
            axum::http::HeaderValue::from_static("task-17"),
        )
        .add_header(
            header("x-attribution-tags"),
            axum::http::HeaderValue::from_static(r#"{"agent":"worker","session":"s-3"}"#),
        )
        .add_header(header("x-project"), axum::http::HeaderValue::from_static("alpha"))
}

/// The one event for the one ledger row, carrying the request's attribution.
async fn assert_one_tagged_event(h: &Harness) -> Value {
    let ledger = common::wait_for_ledger_rows(&*h.db, 1).await;
    let events = h.usage.wait_for(1).await;
    assert_eq!(events.len(), 1, "one ledger row, one event: {events:?}");
    let event = events[0].clone();
    assert_eq!(event["event"], "usage");
    assert_eq!(event["ledger_id"], ledger[0].id);
    assert_eq!(event["model"], ledger[0].model.as_str());
    assert_eq!(event["correlation_id"], "task-17");
    assert_eq!(event["tags"], json!({"agent": "worker", "session": "s-3"}));
    assert_eq!(event["project"], "alpha");
    event
}

fn chat(stream: bool) -> Value {
    json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "hi"}],
        "temperature": 0.0,
        "stream": stream,
    })
}

fn message(stream: bool) -> Value {
    json!({
        "model": "claude-opus-4-5",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
        "stream": stream,
    })
}

#[tokio::test]
async fn non_streaming_chat_emits_a_usage_event() {
    let h = harness(CacheConfig::default()).await;
    assert_eq!(tagged_post(&h, "/v1/chat/completions", chat(false)).await.status_code(), 200);
    let event = assert_one_tagged_event(&h).await;
    assert!(event["cost_usd"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn streaming_chat_emits_a_usage_event() {
    let h = harness(CacheConfig::default()).await;
    let resp = tagged_post(&h, "/v1/chat/completions", chat(true)).await;
    assert_eq!(resp.status_code(), 200);
    assert!(resp.text().contains("[DONE]"));
    assert_one_tagged_event(&h).await;
}

#[tokio::test]
async fn non_streaming_messages_emit_a_usage_event() {
    let h = harness(CacheConfig::default()).await;
    assert_eq!(tagged_post(&h, "/v1/messages", message(false)).await.status_code(), 200);
    let event = assert_one_tagged_event(&h).await;
    assert_eq!(event["provider"], "anthropic");
}

#[tokio::test]
async fn streaming_messages_emit_a_usage_event() {
    let h = harness(CacheConfig::default()).await;
    assert_eq!(tagged_post(&h, "/v1/messages", message(true)).await.status_code(), 200);
    assert_one_tagged_event(&h).await;
}

#[tokio::test]
async fn a_response_cache_hit_emits_a_usage_event_with_its_saving() {
    let h = harness(caching()).await;
    assert_eq!(tagged_post(&h, "/v1/chat/completions", chat(false)).await.status_code(), 200);
    common::wait_for_ledger_rows(&*h.db, 1).await;
    h.usage.wait_for(1).await;

    let hit = tagged_post(&h, "/v1/chat/completions", chat(false)).await;
    assert_eq!(hit.headers().get("x-modelrouter-cache").unwrap(), "HIT");
    let events = h.usage.wait_for(2).await;
    assert_eq!(events.len(), 2);
    let (miss, hit) = (&events[0], &events[1]);
    assert_eq!(hit["cache_hit"], true);
    assert_eq!(hit["cost_usd"], 0.0);
    assert_eq!(hit["saved_usd"], miss["cost_usd"]);
    assert_eq!(hit["uncached_cost_usd"], miss["cost_usd"]);
    assert_eq!(hit["correlation_id"], "task-17");
}

#[tokio::test]
async fn responses_emit_a_usage_event() {
    let h = harness(CacheConfig::default()).await;
    let resp = tagged_post(&h, "/v1/responses", json!({"model": "mock-model", "input": "hi"})).await;
    assert_eq!(resp.status_code(), 200);
    assert_one_tagged_event(&h).await;
}

#[tokio::test]
async fn embeddings_emit_a_usage_event() {
    let h = harness(CacheConfig::default()).await;
    let resp = tagged_post(
        &h,
        "/v1/embeddings",
        json!({"model": "openai/text-embedding-3-small", "input": "hello world"}),
    )
    .await;
    assert_eq!(resp.status_code(), 200);
    assert_one_tagged_event(&h).await;
}

#[tokio::test]
async fn search_emits_a_usage_event() {
    let h = harness(CacheConfig::default()).await;
    let resp = tagged_post(&h, "/v1/search", json!({"query": "rust"})).await;
    assert_eq!(resp.status_code(), 200);
    assert_one_tagged_event(&h).await;
}

#[tokio::test]
async fn a_search_cache_hit_emits_a_usage_event() {
    let h = harness(caching()).await;
    assert_eq!(tagged_post(&h, "/v1/search", json!({"query": "rust"})).await.status_code(), 200);
    common::wait_for_ledger_rows(&*h.db, 1).await;
    h.usage.wait_for(1).await;

    assert_eq!(tagged_post(&h, "/v1/search", json!({"query": "rust"})).await.status_code(), 200);
    let events = h.usage.wait_for(2).await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[1]["cache_hit"], true);
    assert_eq!(events[1]["project"], "alpha");
}

#[tokio::test]
async fn image_generation_emits_a_usage_event() {
    let h = harness(CacheConfig::default()).await;
    let resp = tagged_post(
        &h,
        "/v1/images/generations",
        json!({"model": "dall-e-3", "prompt": "a cat", "n": 1, "quality": "standard"}),
    )
    .await;
    assert_eq!(resp.status_code(), 200);
    assert_one_tagged_event(&h).await;
}

#[tokio::test]
async fn speech_emits_a_usage_event() {
    let h = harness(CacheConfig::default()).await;
    let resp = tagged_post(
        &h,
        "/v1/audio/speech",
        json!({"model": "tts-1", "input": "Hello world", "voice": "alloy"}),
    )
    .await;
    assert_eq!(resp.status_code(), 200);
    assert_one_tagged_event(&h).await;
}

#[tokio::test]
async fn transcription_emits_a_usage_event() {
    use axum_test::multipart::{MultipartForm, Part};
    let h = harness(CacheConfig::default()).await;
    let form = MultipartForm::new()
        .add_part(
            "file",
            Part::bytes(b"RIFF\x00\x00\x00\x00WAVEfmt ".to_vec())
                .file_name("sample.wav")
                .mime_type("audio/wav"),
        )
        .add_text("model", "whisper-1")
        .add_text("response_format", "json");
    let resp = tagged(h.server.post("/v1/audio/transcriptions")).multipart(form).await;
    assert_eq!(resp.status_code(), 200);
    assert_one_tagged_event(&h).await;
}

#[tokio::test]
async fn the_health_probe_emits_no_usage_event() {
    let h = harness(CacheConfig::default()).await;
    assert_eq!(h.server.get("/health/deep").await.status_code(), 200);
    // The probes write their own ledger rows; once they exist, any event
    // would already have been dispatched.
    common::wait_for_ledger_rows(&*h.db, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(h.usage.events().is_empty(), "{:?}", h.usage.events());
}
