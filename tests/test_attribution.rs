//! Request attribution (issue #13): tagging a call with the caller's own unit
//! of work, persisting it on both the prompt and the cost-ledger row, and
//! querying spend + cache savings back out by that tag.

mod common;

use axum_test::TestServer;
use modelrouter::api::admin::auth::{issue_jwt, AdminClaims};
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::{CacheConfig, PricingEntry, Settings};
use modelrouter::db::models::{NewApiKey, NewUser};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::costs::{AttributionFilter, CostRepository};
use modelrouter::db::repositories::prompts::PromptRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::providers::{
    embed_registry::EmbeddingRegistry, registry::ProviderRegistry, search::SearchResultItem,
    search_registry::SearchRegistry,
};
use modelrouter::router::{
    cache::ResponseCache, complexity::ComplexityRouter, cost::CostCalculator,
    engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

const EPOCH: &str = "1970-01-01T00:00:00Z";
const FOREVER: &str = "2999-01-01T00:00:00Z";

async fn build_app(cache: CacheConfig) -> (TestServer, Arc<dyn DatabaseProvider>, Arc<Settings>) {
    let db = common::in_memory_db().await;
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
    // Deterministic, non-zero prices so spend and savings are checkable.
    settings.pricing = vec![
        PricingEntry {
            model: "mock-model".to_string(),
            input_per_million: 1000.0,
            output_per_million: 1000.0,
            ..Default::default()
        },
        PricingEntry {
            model: "search/tavily".to_string(),
            input_per_million: 0.01,
            output_per_million: 0.0,
            ..Default::default()
        },
    ];
    let settings = Arc::new(settings);
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);

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
            common::MockEmbeddingAdapter {
                embedding: vec![0.1_f32, 0.2],
            },
        )),
        search_registry: Arc::new(SearchRegistry::new_with_mock(common::MockSearchAdapter {
            results: vec![SearchResultItem {
                title: "Example Domain".to_string(),
                url: "https://example.com".to_string(),
                snippet: "Example description".to_string(),
                score: Some(0.9),
                published_date: None,
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
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
    };
    (
        TestServer::new(build_router(state)).unwrap(),
        db,
        settings,
    )
}

fn bearer() -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer test-token"),
    )
}

fn admin_jwt(settings: &Settings, role: &str) -> String {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
    issue_jwt(
        &AdminClaims {
            sub: 1,
            name: format!("{}-user", role),
            role: role.to_string(),
            exp,
        },
        &settings.auth.jwt_secret,
    )
    .unwrap()
}

/// Cost logging is fire-and-forget, so poll until the expected number of ledger
/// rows exists rather than sleeping for a guessed interval.
async fn wait_for_ledger_rows(db: &Arc<dyn DatabaseProvider>, want: i64) {
    for _ in 0..200 {
        let rows = CostRepository::list_cost_entries_before(&**db, FOREVER)
            .await
            .unwrap();
        if rows.len() as i64 >= want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {} cost-ledger rows", want);
}

fn attribution_body() -> serde_json::Value {
    json!({
        "correlation_id": "eng-4711-run-3",
        "tags": { "engagement": "eng-4711", "phase": "research" }
    })
}

// ── Round-trip: completions ───────────────────────────────────────────────────

#[tokio::test]
async fn completions_attribution_lands_on_prompt_and_ledger_rows() {
    let (server, db, _settings) = build_app(CacheConfig::default()).await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header(bearer().0, bearer().1)
        .json(&json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "hi"}],
            "attribution": attribution_body(),
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    wait_for_ledger_rows(&db, 1).await;

    let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
    assert_eq!(prompts.len(), 1);
    assert_eq!(
        prompts[0].attribution_correlation_id.as_deref(),
        Some("eng-4711-run-3")
    );
    assert_eq!(
        prompts[0].attribution_tags,
        r#"{"engagement":"eng-4711","phase":"research"}"#
    );

    let ledger = CostRepository::list_cost_entries_before(&*db, FOREVER)
        .await
        .unwrap();
    assert_eq!(ledger.len(), 1);
    assert_eq!(
        ledger[0].attribution_correlation_id.as_deref(),
        Some("eng-4711-run-3")
    );
    assert_eq!(
        ledger[0].attribution_tags,
        r#"{"engagement":"eng-4711","phase":"research"}"#
    );
}

// ── Round-trip: search ────────────────────────────────────────────────────────

#[tokio::test]
async fn search_attribution_lands_on_prompt_and_ledger_rows() {
    let (server, db, _settings) = build_app(CacheConfig::default()).await;

    let resp = server
        .post("/v1/search")
        .add_header(bearer().0, bearer().1)
        .json(&json!({
            "query": "rust programming language",
            "attribution": attribution_body(),
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    wait_for_ledger_rows(&db, 1).await;

    let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
    assert_eq!(
        prompts[0].attribution_correlation_id.as_deref(),
        Some("eng-4711-run-3")
    );
    let ledger = CostRepository::list_cost_entries_before(&*db, FOREVER)
        .await
        .unwrap();
    assert_eq!(
        ledger[0].attribution_tags,
        r#"{"engagement":"eng-4711","phase":"research"}"#
    );
}

#[tokio::test]
async fn attribution_can_arrive_on_headers() {
    let (server, db, _settings) = build_app(CacheConfig::default()).await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header(bearer().0, bearer().1)
        .add_header(
            axum::http::HeaderName::from_static("x-attribution-correlation-id"),
            axum::http::HeaderValue::from_static("hdr-run-1"),
        )
        .add_header(
            axum::http::HeaderName::from_static("x-attribution-tags"),
            axum::http::HeaderValue::from_static(r#"{"engagement":"eng-hdr"}"#),
        )
        .json(&json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    wait_for_ledger_rows(&db, 1).await;
    let ledger = CostRepository::list_cost_entries_before(&*db, FOREVER)
        .await
        .unwrap();
    assert_eq!(
        ledger[0].attribution_correlation_id.as_deref(),
        Some("hdr-run-1")
    );
    assert_eq!(ledger[0].attribution_tags, r#"{"engagement":"eng-hdr"}"#);
}

// ── The cache must not be fragmented by attribution ───────────────────────────

#[test]
fn attribution_does_not_change_the_completion_cache_key() {
    use modelrouter::router::cache::completion_cache_key;

    let base = json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "hi"}],
        "temperature": 0.0,
    });
    let mut tagged_a = base.clone();
    tagged_a["attribution"] = json!({"correlation_id": "run-a", "tags": {"engagement": "a"}});
    let mut tagged_b = base.clone();
    tagged_b["attribution"] = json!({"correlation_id": "run-b", "tags": {"engagement": "b"}});

    let k0 = completion_cache_key("mock-model", &base);
    let ka = completion_cache_key("mock-model", &tagged_a);
    let kb = completion_cache_key("mock-model", &tagged_b);
    assert_eq!(k0, ka, "attribution must not alter the cache key");
    assert_eq!(ka, kb, "two attributions must share one cache entry");

    // Guard the boundary: a field that *does* change the answer still must.
    let mut different = base.clone();
    different["temperature"] = json!(0.9);
    assert_ne!(k0, completion_cache_key("mock-model", &different));
}

#[tokio::test]
async fn two_differently_attributed_requests_share_one_cache_entry() {
    let cache = CacheConfig {
        enabled: true,
        max_entries: 32,
        ttl_seconds: 300,
        ..Default::default()
    };
    let (server, db, _settings) = build_app(cache).await;

    let call = |correlation: &'static str| {
        let server = &server;
        async move {
            server
                .post("/v1/chat/completions")
                .add_header(bearer().0, bearer().1)
                .json(&json!({
                    "model": "mock-model",
                    "messages": [{"role": "user", "content": "hi"}],
                    "temperature": 0.0,
                    "attribution": {"correlation_id": correlation},
                }))
                .await
        }
    };

    let first = call("run-a").await;
    assert_eq!(first.status_code(), 200);
    assert_eq!(first.headers().get("x-modelrouter-cache").unwrap(), "MISS");
    wait_for_ledger_rows(&db, 1).await;

    let second = call("run-b").await;
    assert_eq!(second.status_code(), 200);
    assert_eq!(
        second.headers().get("x-modelrouter-cache").unwrap(),
        "HIT",
        "a request differing only in attribution must hit the cache"
    );
    wait_for_ledger_rows(&db, 2).await;

    // The hit is metered against the *second* caller's unit of work, not the
    // first: savings follow whoever made the call.
    let hit = CostRepository::attribution_totals(
        &*db,
        &AttributionFilter::CorrelationId("run-b".to_string()),
        EPOCH,
        FOREVER,
    )
    .await
    .unwrap();
    assert_eq!(hit.requests, 1);
    assert_eq!(hit.cache_hits, 1);
    assert_eq!(hit.cost_usd, 0.0);
    assert!(hit.saved_usd > 0.0, "cache hit must record a saving");

    let miss = CostRepository::attribution_totals(
        &*db,
        &AttributionFilter::CorrelationId("run-a".to_string()),
        EPOCH,
        FOREVER,
    )
    .await
    .unwrap();
    assert_eq!(miss.cache_hits, 0);
    assert!(miss.cost_usd > 0.0);
}

// ── Validation ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn oversized_or_malformed_attribution_is_rejected() {
    let (server, _db, _settings) = build_app(CacheConfig::default()).await;

    let cases = vec![
        json!("not-an-object"),
        json!({"correlation_id": "x".repeat(200)}),
        json!({"tags": {"k": "v".repeat(200)}}),
        json!({"tags": {"a": 1, "b": 2, "c": 3, "d": 4, "e": 5, "f": 6, "g": 7, "h": 8, "i": 9}}),
        json!({"tags": {"nested": {"deep": true}}}),
        json!({"unknown_field": "x"}),
        json!({"tags": {"bad key": "v"}}),
    ];
    for attribution in cases {
        let resp = server
            .post("/v1/chat/completions")
            .add_header(bearer().0, bearer().1)
            .json(&json!({
                "model": "mock-model",
                "messages": [{"role": "user", "content": "hi"}],
                "attribution": attribution.clone(),
            }))
            .await;
        assert_eq!(
            resp.status_code(),
            400,
            "expected 400 for attribution {}",
            attribution
        );
    }
}

// ── Filtered query ────────────────────────────────────────────────────────────

#[tokio::test]
async fn filtered_query_totals_and_breakdowns_are_correct() {
    let (server, db, _settings) = build_app(CacheConfig::default()).await;

    // Two calls for engagement A (one completion, one search), one for B.
    for _ in 0..1 {
        server
            .post("/v1/chat/completions")
            .add_header(bearer().0, bearer().1)
            .json(&json!({
                "model": "mock-model",
                "messages": [{"role": "user", "content": "hi"}],
                "attribution": {"correlation_id": "run-a", "tags": {"engagement": "A"}},
            }))
            .await;
    }
    server
        .post("/v1/search")
        .add_header(bearer().0, bearer().1)
        .json(&json!({
            "query": "rust",
            "attribution": {"correlation_id": "run-a", "tags": {"engagement": "A"}},
        }))
        .await;
    server
        .post("/v1/chat/completions")
        .add_header(bearer().0, bearer().1)
        .json(&json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "other"}],
            "attribution": {"correlation_id": "run-b", "tags": {"engagement": "B"}},
        }))
        .await;
    wait_for_ledger_rows(&db, 3).await;

    let by_tag = AttributionFilter::Tag {
        key: "engagement".to_string(),
        value: "A".to_string(),
    };
    let totals = CostRepository::attribution_totals(&*db, &by_tag, EPOCH, FOREVER)
        .await
        .unwrap();
    assert_eq!(totals.requests, 2, "engagement A made two calls");
    assert!(totals.cost_usd > 0.0);

    // The same rows, reached by correlation id, must agree.
    let by_corr = CostRepository::attribution_totals(
        &*db,
        &AttributionFilter::CorrelationId("run-a".to_string()),
        EPOCH,
        FOREVER,
    )
    .await
    .unwrap();
    assert_eq!(by_corr, totals);

    // B's spend is excluded, and the two slices sum to the whole.
    let b = CostRepository::attribution_totals(
        &*db,
        &AttributionFilter::Tag {
            key: "engagement".to_string(),
            value: "B".to_string(),
        },
        EPOCH,
        FOREVER,
    )
    .await
    .unwrap();
    assert_eq!(b.requests, 1);
    let all = CostRepository::list_cost_entries_before(&*db, FOREVER)
        .await
        .unwrap();
    assert_eq!(totals.requests + b.requests, all.len() as i64);

    // Breakdown by model: one completion row + one search row for A.
    let by_model = CostRepository::attribution_by_model(&*db, &by_tag, EPOCH, FOREVER)
        .await
        .unwrap();
    assert_eq!(by_model.len(), 2);
    let models: Vec<&str> = by_model.iter().map(|r| r.key.as_str()).collect();
    assert!(models.contains(&"search/tavily"), "got {:?}", models);
    let summed: i64 = by_model.iter().map(|r| r.totals.requests).sum();
    assert_eq!(summed, totals.requests);

    // Breakdown by day: everything happened today, so exactly one bucket.
    let by_day = CostRepository::attribution_by_day(&*db, &by_tag, EPOCH, FOREVER)
        .await
        .unwrap();
    assert_eq!(by_day.len(), 1);
    assert_eq!(by_day[0].totals.requests, totals.requests);

    // Facets feed the dashboard pickers.
    let keys = CostRepository::distinct_attribution_tag_keys(&*db)
        .await
        .unwrap();
    assert_eq!(keys, vec!["engagement".to_string()]);
    let values = CostRepository::distinct_attribution_values(&*db, Some("engagement"), 100)
        .await
        .unwrap();
    assert_eq!(values, vec!["A".to_string(), "B".to_string()]);
    let corr = CostRepository::distinct_attribution_values(&*db, None, 100)
        .await
        .unwrap();
    assert_eq!(corr, vec!["run-a".to_string(), "run-b".to_string()]);
}

#[tokio::test]
async fn untagged_calls_are_not_attributed_to_anyone() {
    let (server, db, _settings) = build_app(CacheConfig::default()).await;
    server
        .post("/v1/chat/completions")
        .add_header(bearer().0, bearer().1)
        .json(&json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .await;
    wait_for_ledger_rows(&db, 1).await;

    let ledger = CostRepository::list_cost_entries_before(&*db, FOREVER)
        .await
        .unwrap();
    assert!(ledger[0].attribution_correlation_id.is_none());
    assert_eq!(ledger[0].attribution_tags, "{}");

    let totals = CostRepository::attribution_totals(
        &*db,
        &AttributionFilter::Tag {
            key: "engagement".to_string(),
            value: "A".to_string(),
        },
        EPOCH,
        FOREVER,
    )
    .await
    .unwrap();
    assert_eq!(totals.requests, 0);
    assert_eq!(totals.cost_usd, 0.0);
}

// ── Admin API ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn admin_usage_endpoint_requires_authentication() {
    let (server, _db, _settings) = build_app(CacheConfig::default()).await;
    let resp = server
        .get("/admin/api/usage/attribution")
        .add_query_param("value", "run-a")
        .await;
    assert_eq!(resp.status_code(), 401);

    let resp = server.get("/admin/api/usage/attribution/facets").await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn admin_usage_endpoint_rejects_a_forged_token() {
    let (server, _db, _settings) = build_app(CacheConfig::default()).await;
    let resp = server
        .get("/admin/api/usage/attribution")
        .add_query_param("value", "run-a")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer not-a-real-jwt"),
        )
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn admin_usage_endpoint_returns_spend_and_savings() {
    let (server, db, settings) = build_app(CacheConfig::default()).await;
    server
        .post("/v1/chat/completions")
        .add_header(bearer().0, bearer().1)
        .json(&json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "hi"}],
            "attribution": {"correlation_id": "run-a", "tags": {"engagement": "A"}},
        }))
        .await;
    wait_for_ledger_rows(&db, 1).await;

    let token = admin_jwt(&settings, "viewer");
    let resp = server
        .get("/admin/api/usage/attribution")
        .add_query_param("key", "engagement")
        .add_query_param("value", "A")
        .add_query_param("window", "all")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
        )
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["filter"], "engagement=A");
    assert_eq!(body["totals"]["requests"], 1);
    assert_eq!(body["totals"]["cache_hits"], 0);
    assert!(body["totals"]["cost_usd"].as_f64().unwrap() > 0.0);
    assert_eq!(body["by_model"].as_array().unwrap().len(), 1);
    assert_eq!(body["by_day"].as_array().unwrap().len(), 1);

    // A missing value is a client error, not an empty report.
    let resp = server
        .get("/admin/api/usage/attribution")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
        )
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn admin_facets_endpoint_lists_keys_and_values() {
    let (server, db, settings) = build_app(CacheConfig::default()).await;
    server
        .post("/v1/chat/completions")
        .add_header(bearer().0, bearer().1)
        .json(&json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "hi"}],
            "attribution": {"correlation_id": "run-a", "tags": {"engagement": "A"}},
        }))
        .await;
    wait_for_ledger_rows(&db, 1).await;

    let token = admin_jwt(&settings, "viewer");
    let auth = (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
    );

    let resp = server
        .get("/admin/api/usage/attribution/facets")
        .add_header(auth.0.clone(), auth.1.clone())
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["tag_keys"][0], "engagement");
    assert_eq!(body["correlation_ids"][0], "run-a");

    let resp = server
        .get("/admin/api/usage/attribution/facets")
        .add_query_param("key", "engagement")
        .add_header(auth.0.clone(), auth.1.clone())
        .await;
    let body: serde_json::Value = resp.json();
    assert_eq!(body["values"][0], "A");

    // An unsafe key never reaches the JSON path.
    let resp = server
        .get("/admin/api/usage/attribution/facets")
        .add_query_param("key", "bad\"key")
        .add_header(auth.0, auth.1)
        .await;
    assert_eq!(resp.status_code(), 400);
}

// ── Dashboard ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn reports_page_filters_by_attribution() {
    let (server, db, settings) = build_app(CacheConfig::default()).await;
    server
        .post("/v1/chat/completions")
        .add_header(bearer().0, bearer().1)
        .json(&json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "hi"}],
            "attribution": {"correlation_id": "run-a", "tags": {"engagement": "A"}},
        }))
        .await;
    wait_for_ledger_rows(&db, 1).await;

    let token = admin_jwt(&settings, "viewer");
    let cookie = axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap();

    // Unauthenticated operators get bounced.
    let resp = server.get("/admin/reports/panels").await;
    assert_ne!(resp.status_code(), 200);

    // The picker options come from the ledger.
    let resp = server
        .get("/admin/reports")
        .add_header(axum::http::header::COOKIE, cookie.clone())
        .await;
    assert_eq!(resp.status_code(), 200);
    assert!(resp.text().contains("tag: engagement"));

    // Selecting a value swaps in the attributed-usage panel.
    let resp = server
        .get("/admin/reports/panels")
        .add_query_param("key", "engagement")
        .add_query_param("value", "A")
        .add_query_param("window", "monthly")
        .add_header(axum::http::header::COOKIE, cookie.clone())
        .await;
    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("Attributed usage — engagement=A"), "{}", html);
    assert!(html.contains("Saved by cache"));

    // With no value selected, the ordinary reports panels still render.
    let resp = server
        .get("/admin/reports/panels")
        .add_query_param("window", "monthly")
        .add_header(axum::http::header::COOKIE, cookie)
        .await;
    assert_eq!(resp.status_code(), 200);
    assert!(resp.text().contains("Spend by User"));
}
