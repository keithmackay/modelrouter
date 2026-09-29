//! Issue #86: OpenAI `image_url` content parts reach Claude on Vertex as
//! native Anthropic `image` blocks through every OpenAI-shaped ingress —
//! `/v1/chat/completions` (non-streaming and streaming) and `/v1/responses` —
//! driven end to end through the real `VertexAdapter` against a local server
//! speaking the Vertex wire shape. Vertex accepts only base64 image sources, so
//! an image URL is refused with a clear error and never forwarded.
#![cfg(feature = "vertex")]

mod common;

use axum::routing::post;
use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::Settings;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::providers::vertex::auth::StaticTokenProvider;
use modelrouter::providers::vertex::VertexAdapter;
use modelrouter::router::{
    cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type Seen = Arc<Mutex<Vec<serde_json::Value>>>;

const MODEL: &str = "vertex/claude-sonnet-4-5";

/// A Claude-on-Vertex server recording every body; answers SSE when the body
/// asks for a stream and one JSON message otherwise.
async fn serve_vertex_claude() -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let router = axum::Router::new().fallback(post(
        move |axum::Json(body): axum::Json<serde_json::Value>| {
            let sink = sink.clone();
            async move {
                let streaming = body["stream"] == true;
                sink.lock().unwrap().push(body);
                if streaming {
                    "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"seen\"}}\n\
                     data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n"
                        .to_string()
                } else {
                    "{\"content\":[{\"type\":\"text\",\"text\":\"seen\"}],\"stop_reason\":\"end_turn\",\
                     \"usage\":{\"input_tokens\":3,\"output_tokens\":1}}"
                        .to_string()
                }
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), seen)
}

async fn app(vertex_base: &str) -> TestServer {
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;
    let adapter = VertexAdapter::with_token_provider(
        "proj".into(),
        "global".into(),
        Arc::new(StaticTokenProvider::new("tok".into())),
        10,
    )
    .unwrap()
    .with_api_base(vertex_base.to_string());

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let state = AppState {
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        router: Arc::new(RequestRouter::new(settings.clone())),
        settings,
        db: db.clone(),
        pool: None,
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(adapter)),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
        response_cache: Arc::new(modelrouter::router::cache::ResponseCache::new(
            &modelrouter::config::schema::CacheConfig::default(),
        )),
        embedding_registry: Arc::new(
            modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
                common::MockEmbeddingAdapter {
                    embedding: vec![0.1_f32, 0.2],
                },
            ),
        ),
        search_registry: Arc::new(
            modelrouter::providers::search_registry::SearchRegistry::new(HashMap::new()),
        ),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            HashMap::new(),
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
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    TestServer::new(build_router(state)).unwrap()
}

fn auth() -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer test-token"),
    )
}

fn image_messages(url: &str) -> serde_json::Value {
    serde_json::json!([{
        "role": "user",
        "content": [
            {"type": "text", "text": "Describe this diagram."},
            {"type": "image_url", "image_url": {"url": url}}
        ]
    }])
}

/// The one body Vertex received carries the image as a base64 source.
fn assert_vertex_got_base64_image(seen: &Seen) {
    let bodies = seen.lock().unwrap();
    assert_eq!(bodies.len(), 1, "exactly one Vertex call");
    let blocks = bodies[0]["messages"][0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "image", "{}", bodies[0]);
    assert_eq!(blocks[1]["source"]["type"], "base64");
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    assert_eq!(blocks[1]["source"]["data"], "iVBORw0KGgo=");
}

const DATA_URL: &str = "data:image/png;base64,iVBORw0KGgo=";
const HTTPS_URL: &str = "https://example.com/diagram.png";

#[tokio::test]
async fn chat_completions_non_streaming_translates_image_url() {
    let (base, seen) = serve_vertex_claude().await;
    let (hk, hv) = auth();
    let resp = app(&base)
        .await
        .post("/v1/chat/completions")
        .add_header(hk, hv)
        .json(&serde_json::json!({"model": MODEL, "messages": image_messages(DATA_URL)}))
        .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert_eq!(
        resp.json::<serde_json::Value>()["choices"][0]["message"]["content"],
        "seen"
    );
    assert_vertex_got_base64_image(&seen);
}

#[tokio::test]
async fn chat_completions_streaming_translates_image_url() {
    let (base, seen) = serve_vertex_claude().await;
    let (hk, hv) = auth();
    let resp = app(&base)
        .await
        .post("/v1/chat/completions")
        .add_header(hk, hv)
        .json(&serde_json::json!({
            "model": MODEL, "stream": true, "messages": image_messages(DATA_URL)
        }))
        .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert!(resp.text().contains("seen"), "{}", resp.text());
    assert_eq!(seen.lock().unwrap()[0]["stream"], true);
    assert_vertex_got_base64_image(&seen);
}

#[tokio::test]
async fn responses_translates_chat_and_responses_style_image_parts() {
    // `/v1/responses` forwards `input` as messages: both the chat-style
    // `image_url` part and the Responses-native `input_image` part translate.
    let (base, seen) = serve_vertex_claude().await;
    let server = app(&base).await;
    let inputs = [
        image_messages(DATA_URL),
        serde_json::json!([{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "Describe this diagram."},
                {"type": "input_image", "image_url": DATA_URL}
            ]
        }]),
    ];
    for input in inputs {
        seen.lock().unwrap().clear();
        let (hk, hv) = auth();
        let resp = server
            .post("/v1/responses")
            .add_header(hk, hv)
            .json(&serde_json::json!({"model": MODEL, "input": input}))
            .await;
        assert_eq!(resp.status_code(), 200, "{}", resp.text());
        assert_vertex_got_base64_image(&seen);
    }
}

#[tokio::test]
async fn image_urls_are_refused_on_every_ingress_without_calling_vertex() {
    let (base, seen) = serve_vertex_claude().await;
    let server = app(&base).await;
    let requests = [
        (
            "/v1/chat/completions",
            serde_json::json!({"model": MODEL, "messages": image_messages(HTTPS_URL)}),
        ),
        (
            "/v1/chat/completions",
            serde_json::json!({"model": MODEL, "stream": true, "messages": image_messages(HTTPS_URL)}),
        ),
        (
            "/v1/responses",
            serde_json::json!({"model": MODEL, "input": image_messages(HTTPS_URL)}),
        ),
    ];
    for (path, body) in requests {
        let (hk, hv) = auth();
        let resp = server.post(path).add_header(hk, hv).json(&body).await;
        assert!(!resp.status_code().is_success(), "{path}: {}", resp.text());
        assert!(
            resp.text().contains("only base64 image sources"),
            "{path}: the error must name the cause: {}",
            resp.text()
        );
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "no request may reach Vertex"
    );
}
