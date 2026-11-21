//! Every request-path cost-ledger row emits exactly one `usage` callback event
//! carrying the request's attribution: streaming and non-streaming chat, the
//! Messages API, response-cache hits, /v1/responses, embeddings, search,
//! images and audio. Rerank and decisions are covered in their own suites.
//! The health probe's synthetic rows emit none. `GET /admin/api/usage/events`
//! reads the same events back by ledger id.

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
    settings: Arc<Settings>,
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
        settings,
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

// ── Reading events back ───────────────────────────────────────────────────────

const EVENTS: &str = "/admin/api/usage/events";

fn admin(h: &Harness, request: axum_test::TestRequest) -> axum_test::TestRequest {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
    let token = modelrouter::api::admin::auth::issue_jwt(
        &modelrouter::api::admin::auth::AdminClaims {
            sub: 1,
            name: "viewer-user".to_string(),
            role: "viewer".to_string(),
            exp,
        },
        &h.settings.auth.jwt_secret,
    )
    .unwrap();
    request.add_header(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

/// GET the events page for `params`, expecting a 200.
async fn read_events(h: &Harness, params: &[(&str, &str)]) -> Value {
    let mut request = h.server.get(EVENTS);
    for (name, value) in params {
        request = request.add_query_param(name, value);
    }
    let resp = admin(h, request).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    resp.json()
}

/// Write `n` request-path ledger rows straight to the store.
async fn ledger_rows(h: &Harness, n: usize) -> Vec<modelrouter::db::models::CostLedgerEntry> {
    use modelrouter::db::repositories::costs::CostRepository;
    let mut rows = Vec::new();
    for i in 0..n {
        let entry = modelrouter::db::models::NewCostLedgerEntry {
            call_kind: modelrouter::db::models::CallKind::Chat,
            user_id: 1,
            prompt_id: None,
            model: format!("m{i}"),
            provider: "p".into(),
            project: None,
            tokens_in: 10,
            tokens_out: 5,
            cost_usd: 0.01,
            api_key_id: None,
            attribution_correlation_id: None,
            attribution_tags: "{}".into(),
            experiment_id: None,
            experiment_variant: None,
            tokens_estimated: false,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            request_fingerprint: None,
        };
        rows.push(CostRepository::create(&*h.db, entry).await.unwrap());
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    rows
}

fn ledger_ids(page: &Value) -> Vec<i64> {
    page["events"].as_array().unwrap().iter().map(|e| e["ledger_id"].as_i64().unwrap()).collect()
}

#[tokio::test]
async fn reading_events_needs_an_admin_session() {
    let h = harness(CacheConfig::default()).await;
    assert_eq!(h.server.get(EVENTS).await.status_code(), 401);
    let forged = h.server.get(EVENTS).add_header(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer not-a-real-jwt"),
    );
    assert_eq!(forged.await.status_code(), 401);
}

#[tokio::test]
async fn an_event_read_back_is_the_emitted_payload() {
    let h = harness(CacheConfig::default()).await;
    assert_eq!(tagged_post(&h, "/v1/chat/completions", chat(false)).await.status_code(), 200);
    let emitted = assert_one_tagged_event(&h).await;

    // Both sides cross the wire as JSON text; parsing that text is not exact
    // for every float, so compare the emitted payload as a receiver parses it.
    let delivered: Value = serde_json::from_str(&emitted.to_string()).unwrap();
    let page = read_events(&h, &[]).await;
    assert_eq!(page["events"], json!([delivered]));
    assert!(page["events"][0].get("delivery_id").is_none());
    assert_eq!(page["next_after_id"], Value::Null);
}

#[tokio::test]
async fn pages_cover_every_row_once_in_id_order() {
    let h = harness(CacheConfig::default()).await;
    let rows = ledger_rows(&h, 5).await;
    let all: Vec<i64> = rows.iter().map(|r| r.id).collect();

    let mut seen = Vec::new();
    let mut after = "0".to_string();
    let mut cursors = Vec::new();
    loop {
        let page = read_events(&h, &[("after_id", &after), ("limit", "2")]).await;
        seen.extend(ledger_ids(&page));
        cursors.push(page["next_after_id"].clone());
        match page["next_after_id"].as_i64() {
            Some(next) => after = next.to_string(),
            None => break,
        }
    }
    assert_eq!(seen, all);
    assert_eq!(cursors, [json!(all[1]), json!(all[3]), Value::Null]);

    // A full last page points past itself; the page after it is empty and caught up.
    let full = read_events(&h, &[("limit", "5")]).await;
    assert_eq!(ledger_ids(&full), all);
    assert_eq!(full["next_after_id"], all[4]);
    let empty = read_events(&h, &[("after_id", &all[4].to_string()), ("limit", "5")]).await;
    assert_eq!(empty, json!({"events": [], "next_after_id": null}));

    // Without a limit, a page holds up to 1000 events.
    let default = read_events(&h, &[]).await;
    assert_eq!(ledger_ids(&default), all);
    assert_eq!(default["next_after_id"], Value::Null);
}

#[tokio::test]
async fn since_keeps_rows_from_that_instant_on_in_any_offset() {
    let h = harness(CacheConfig::default()).await;
    let rows = ledger_rows(&h, 4).await;

    let page = read_events(&h, &[("since", &rows[2].created_at)]).await;
    assert_eq!(ledger_ids(&page), [rows[2].id, rows[3].id]);

    let elsewhere = chrono::DateTime::parse_from_rfc3339(&rows[2].created_at)
        .unwrap()
        .with_timezone(&chrono::FixedOffset::east_opt(2 * 3600).unwrap())
        .to_rfc3339();
    let page = read_events(&h, &[("since", &elsewhere)]).await;
    assert_eq!(ledger_ids(&page), [rows[2].id, rows[3].id]);

    let page = read_events(&h, &[("since", &rows[2].created_at), ("after_id", &rows[2].id.to_string())]).await;
    assert_eq!(ledger_ids(&page), [rows[3].id]);
}

#[tokio::test]
async fn bad_parameters_are_refused_naming_the_field() {
    let h = harness(CacheConfig::default()).await;
    for (name, value) in [
        ("limit", "0"),
        ("limit", "1001"),
        ("limit", "ten"),
        ("after_id", "x"),
        ("since", "yesterday"),
        ("since", "2026-11-14"),
    ] {
        let resp = admin(&h, h.server.get(EVENTS).add_query_param(name, value)).await;
        assert_eq!(resp.status_code(), 400, "{name}={value}");
        let body: Value = resp.json();
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains(name), "{name}={value}: {message}");
    }
    for limit in ["1", "1000"] {
        read_events(&h, &[("limit", limit)]).await;
    }
}

#[tokio::test]
async fn health_probe_rows_are_not_read_back() {
    let h = harness(CacheConfig::default()).await;
    assert_eq!(h.server.get("/health/deep").await.status_code(), 200);
    common::wait_for_ledger_rows(&*h.db, 1).await;
    let rows = ledger_rows(&h, 1).await;

    let page = read_events(&h, &[]).await;
    assert_eq!(ledger_ids(&page), [rows[0].id]);
}
