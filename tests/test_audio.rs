//! HTTP handler tests for /v1/audio/* endpoints.

mod common;

use axum_test::multipart::{MultipartForm, Part};
use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::{ProviderConfig, Settings};
use modelrouter::db::models::{NewBudgetRule, NewUser};
use modelrouter::db::repositories::budgets::BudgetRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine};
use std::collections::HashMap;
use std::sync::Arc;

/// Knobs for the handful of tests that need an app differing from the default
/// mock-server wiring. `Default` reproduces that wiring exactly.
#[derive(Default)]
struct AudioAppOpts {
    /// Trip the circuit breaker for the `openai` provider before the request.
    open_circuit: bool,
    /// Point `prompt_db` at a schema-less database so `PromptRepository::create`
    /// fails. Exercises the "prompt row optional, cost row not" storage policy.
    broken_prompt_db: bool,
    /// Deny this model via a budget rule's `model_deny` list.
    deny_model: Option<String>,
    /// Turn off prompt logging, so the storage policy skips the insert
    /// entirely rather than attempting and failing it.
    disable_prompt_storage: bool,
    /// Grant a concurrency budget the request can actually acquire, so the
    /// permit-acquired branch runs rather than the limit-exceeded one.
    max_concurrent: Option<i64>,
}

async fn test_app_with_mock_server() -> (TestServer, Arc<dyn DatabaseProvider>, common::mock_audio::MockAudioServer) {
    let mock_server = common::mock_audio::MockAudioServer::start().await;
    let (server, db) =
        build_audio_app(mock_server.base_url(), AudioAppOpts::default()).await;
    (server, db, mock_server)
}

/// Build an audio-capable app whose `openai` provider points at `api_base`.
async fn build_audio_app(
    api_base: String,
    opts: AudioAppOpts,
) -> (TestServer, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;
    let user_id = common::create_user(&db, "test-user", "test-token").await;

    if opts.deny_model.is_some() || opts.max_concurrent.is_some() {
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
                max_concurrent: opts.max_concurrent,
                model_allow: vec![],
                model_deny: opts.deny_model.iter().cloned().collect(),
                window_start: None,
                window_end: None,
            },
        )
        .await
        .unwrap();
    }

    let mut providers = HashMap::new();
    providers.insert("openai".to_string(), ProviderConfig {
        api_key: "test-key".to_string(),
        api_base: Some(api_base),
        timeout_secs: 10,
        ..Default::default()
    });

    let settings = Arc::new(Settings {
        providers,
        ..Default::default()
    });

    let db: Arc<dyn DatabaseProvider> = Arc::new(db);

    // A connection with no migrations run: the `prompts` table is absent, so
    // every prompt insert errors while the cost ledger (on `db`) still works.
    let prompt_db: Arc<dyn DatabaseProvider> = if opts.broken_prompt_db {
        Arc::new(
            modelrouter::db::sqlite::SqliteDb::connect(":memory:")
                .await
                .unwrap(),
        )
    } else {
        db.clone()
    };

    let circuit_breaker = Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default());
    if opts.open_circuit {
        // The default breaker trips at 5 consecutive failures.
        for _ in 0..5 {
            circuit_breaker.record_failure("openai");
        }
    }

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
            response: "ok".to_string(),
        })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
        response_cache: Arc::new(modelrouter::router::cache::ResponseCache::new(
            &modelrouter::config::schema::CacheConfig::default(),
        )),
        embedding_registry: Arc::new(
            modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
                common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
            ),
        ),
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(
            std::collections::HashMap::new(),
        )),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker,
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(
            modelrouter::config::schema::StorageConfig {
                store_prompts: !opts.disable_prompt_storage,
                ..Default::default()
            },
        )),
        prompt_db,
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };

    (TestServer::new(build_router(state)).unwrap(), db)
}

/// A TCP port that nothing is listening on, for exercising connection-error
/// paths without leaving the host.
async fn dead_base_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

/// A minimal well-formed transcription request: one audio file plus the
/// text fields the OpenAI API expects.
fn transcription_form() -> MultipartForm {
    MultipartForm::new()
        .add_part(
            "file",
            Part::bytes(b"RIFF\x00\x00\x00\x00WAVEfmt ".to_vec())
                .file_name("sample.wav")
                .mime_type("audio/wav"),
        )
        .add_text("model", "whisper-1")
        .add_text("response_format", "json")
}

fn bearer(token: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
    )
}

#[tokio::test]
async fn audio_speech_unauthenticated_returns_401() {
    let (server, _db, _mock) = test_app_with_mock_server().await;
    let resp = server
        .post("/v1/audio/speech")
        .json(&serde_json::json!({"model": "tts-1", "input": "Hello world", "voice": "alloy"}))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn speech_success_path() {
    let (server, db, mock) = test_app_with_mock_server().await;
    let resp = server
        .post("/v1/audio/speech")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": "Hello world",
            "voice": "alloy"
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap(), "audio/mpeg");

    // Provider was called
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/audio/speech");

    // Cost logging happened (give spawn a moment)
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger.len(), 1);
    assert!(ledger[0].cost_usd > 0.0);
}

#[tokio::test]
async fn speech_provider_error_passthrough() {
    let (server, _db, mock) = test_app_with_mock_server().await;
    mock.set_speech_error(axum::http::StatusCode::TOO_MANY_REQUESTS);

    let resp = server
        .post("/v1/audio/speech")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": "Test",
            "voice": "alloy"
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn speech_circuit_breaker_records_failure() {
    let (server, _db, mock) = test_app_with_mock_server().await;
    mock.set_speech_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR);

    let resp = server
        .post("/v1/audio/speech")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": "Test",
            "voice": "alloy"
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 500);
}

#[tokio::test]
async fn speech_rejects_experiment_header() {
    let (server, _db, _mock) = test_app_with_mock_server().await;
    let resp = server
        .post("/v1/audio/speech")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-experiment"),
            axum::http::HeaderValue::from_static("exp-1"),
        )
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": "Hello",
            "voice": "alloy"
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn speech_policy_concurrency_limit_exceeded() {
    let db = common::in_memory_db().await;
    let user_id = common::create_user(&db, "test-user", "test-token").await;
    let mock_server = common::mock_audio::MockAudioServer::start().await;

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
            max_concurrent: Some(1),
            model_allow: vec![],
            model_deny: vec![],
            window_start: None,
            window_end: None,
        },
    )
    .await
    .unwrap();

    let mut providers = HashMap::new();
    providers.insert("openai".to_string(), ProviderConfig {
        api_key: "test-key".to_string(),
        api_base: Some(mock_server.base_url()),
        timeout_secs: 10,
        ..Default::default()
    });

    let settings = Arc::new(Settings {
        providers,
        ..Default::default()
    });

    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let concurrency = Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new());
    let _permit = concurrency.try_acquire(user_id, 1).unwrap();

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
            response: "ok".to_string(),
        })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
        response_cache: Arc::new(modelrouter::router::cache::ResponseCache::new(
            &modelrouter::config::schema::CacheConfig::default(),
        )),
        embedding_registry: Arc::new(
            modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
                common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
            ),
        ),
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(
            std::collections::HashMap::new(),
        )),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency,
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    let server = TestServer::new(build_router(state)).unwrap();

    let resp = server
        .post("/v1/audio/speech")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": "Hello",
            "voice": "alloy"
        }))
        .await;
    assert_eq!(resp.status_code(), 429);
}

#[tokio::test]
async fn transcriptions_unauthenticated_returns_401() {
    let (server, _db, _mock) = test_app_with_mock_server().await;
    let resp = server.post("/v1/audio/transcriptions").await;
    assert_eq!(resp.status_code(), 401);
}


#[tokio::test]
async fn transcriptions_rejects_experiment_header() {
    let (server, _db, _mock) = test_app_with_mock_server().await;
    let resp = server
        .post("/v1/audio/transcriptions")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-experiment"),
            axum::http::HeaderValue::from_static("exp-1"),
        )
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn transcriptions_success_forwards_multipart_and_logs_cost() {
    let (server, db, mock) = test_app_with_mock_server().await;
    let resp = server
        .post("/v1/audio/transcriptions")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .multipart(transcription_form())
        .await;
    assert_eq!(resp.status_code(), 200);
    assert_eq!(resp.json::<serde_json::Value>()["text"], "mock transcription");

    // The router reassembled the form and forwarded it as multipart, keeping
    // the file name, the audio bytes and the sibling text fields intact.
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/audio/transcriptions");
    let ct = requests[0].content_type.as_deref().unwrap();
    assert!(ct.starts_with("multipart/form-data"), "got {ct}");
    let body = String::from_utf8_lossy(requests[0].raw_body.as_ref().unwrap()).to_string();
    assert!(body.contains("sample.wav"), "file name not forwarded");
    assert!(body.contains("WAVEfmt"), "audio bytes not forwarded");
    assert!(body.contains("whisper-1"), "model field not forwarded");
    assert!(body.contains("response_format"), "format field not forwarded");

    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].model, "whisper-1");
    assert_eq!(ledger[0].tokens_in, 1);
    assert!(ledger[0].cost_usd > 0.0);
}

#[tokio::test]
async fn transcriptions_text_only_form_without_filename_or_mime() {
    // Parts with neither a file name nor a content type take the other side of
    // the two `Option` branches in the form-reassembly loop.
    let (server, db, mock) = test_app_with_mock_server().await;
    let form = MultipartForm::new()
        .add_text("model", "whisper-1")
        .add_text("language", "en");

    let resp = server
        .post("/v1/audio/transcriptions")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .multipart(form)
        .await;
    assert_eq!(resp.status_code(), 200);
    assert_eq!(mock.requests().len(), 1);
    common::wait_for_ledger_rows(&*db, 1).await;
}

#[tokio::test]
async fn transcriptions_records_attribution_from_headers() {
    // Multipart bodies carry no JSON to read attribution from, so the handler
    // must pick it up from headers alone.
    let (server, db, _mock) = test_app_with_mock_server().await;
    let resp = server
        .post("/v1/audio/transcriptions")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-attribution-correlation-id"),
            axum::http::HeaderValue::from_static("corr-transcribe-1"),
        )
        .multipart(transcription_form())
        .await;
    assert_eq!(resp.status_code(), 200);

    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(
        ledger[0].attribution_correlation_id.as_deref(),
        Some("corr-transcribe-1")
    );
}

#[tokio::test]
async fn transcriptions_provider_error_passthrough() {
    let (server, _db, mock) = test_app_with_mock_server().await;
    mock.set_transcription_error(axum::http::StatusCode::TOO_MANY_REQUESTS, "rate limit");

    let resp = server
        .post("/v1/audio/transcriptions")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .multipart(transcription_form())
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn transcriptions_non_json_response_is_provider_error() {
    // OpenAI returns bare text for response_format=text/srt/vtt; this handler
    // only parses JSON, so such a body surfaces as a provider error rather
    // than being passed through.
    let (server, _db, mock) = test_app_with_mock_server().await;
    mock.set_transcription_raw(
        axum::http::StatusCode::OK,
        "text/plain",
        "a plain transcript, not JSON",
    );

    let resp = server
        .post("/v1/audio/transcriptions")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .multipart(transcription_form())
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn transcriptions_circuit_breaker_open_returns_error() {
    let mock = common::mock_audio::MockAudioServer::start().await;
    let (server, _db) = build_audio_app(
        mock.base_url(),
        AudioAppOpts { open_circuit: true, ..Default::default() },
    )
    .await;

    let resp = server
        .post("/v1/audio/transcriptions")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .multipart(transcription_form())
        .await;
    assert!(resp.status_code().as_u16() >= 400);
    // Short-circuited before any provider call.
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn transcriptions_network_error_returns_provider_error() {
    let (server, _db) = build_audio_app(dead_base_url().await, AudioAppOpts::default()).await;

    let resp = server
        .post("/v1/audio/transcriptions")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .multipart(transcription_form())
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn transcriptions_malformed_multipart_body_rejected() {
    let (server, _db, _mock) = test_app_with_mock_server().await;
    let resp = server
        .post("/v1/audio/transcriptions")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("multipart/form-data; boundary=zzz"),
        )
        .text("not a valid multipart payload")
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn transcriptions_policy_denies_model() {
    let mock = common::mock_audio::MockAudioServer::start().await;
    let (server, _db) = build_audio_app(
        mock.base_url(),
        AudioAppOpts { deny_model: Some("whisper-1".to_string()), ..Default::default() },
    )
    .await;

    let resp = server
        .post("/v1/audio/transcriptions")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .multipart(transcription_form())
        .await;
    assert!(resp.status_code().as_u16() >= 400);
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn transcriptions_prompt_storage_failure_still_records_cost() {
    // Storage policy: the prompt row is optional, the cost row is not. With an
    // unwritable prompt store the request still succeeds and still bills.
    let mock = common::mock_audio::MockAudioServer::start().await;
    let (server, db) = build_audio_app(
        mock.base_url(),
        AudioAppOpts { broken_prompt_db: true, ..Default::default() },
    )
    .await;

    let resp = server
        .post("/v1/audio/transcriptions")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .multipart(transcription_form())
        .await;
    assert_eq!(resp.status_code(), 200);

    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger.len(), 1);
    assert!(ledger[0].cost_usd > 0.0);
    // No prompt row was written, so the ledger entry references none (0).
    assert_eq!(ledger[0].prompt_id, 0);
}

#[tokio::test]
async fn speech_circuit_breaker_open_returns_error() {
    let mock = common::mock_audio::MockAudioServer::start().await;
    let (server, _db) = build_audio_app(
        mock.base_url(),
        AudioAppOpts { open_circuit: true, ..Default::default() },
    )
    .await;

    let resp = server
        .post("/v1/audio/speech")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({"model": "tts-1", "input": "Hello", "voice": "alloy"}))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn speech_network_error_returns_provider_error() {
    let (server, _db) = build_audio_app(dead_base_url().await, AudioAppOpts::default()).await;

    let resp = server
        .post("/v1/audio/speech")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({"model": "tts-1", "input": "Hello", "voice": "alloy"}))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn speech_policy_denies_model() {
    let mock = common::mock_audio::MockAudioServer::start().await;
    let (server, _db) = build_audio_app(
        mock.base_url(),
        AudioAppOpts { deny_model: Some("tts-1".to_string()), ..Default::default() },
    )
    .await;

    let resp = server
        .post("/v1/audio/speech")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({"model": "tts-1", "input": "Hello", "voice": "alloy"}))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn speech_prompt_storage_failure_still_records_cost() {
    let mock = common::mock_audio::MockAudioServer::start().await;
    let (server, db) = build_audio_app(
        mock.base_url(),
        AudioAppOpts { broken_prompt_db: true, ..Default::default() },
    )
    .await;

    let resp = server
        .post("/v1/audio/speech")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({"model": "tts-1", "input": "Hello world", "voice": "alloy"}))
        .await;
    assert_eq!(resp.status_code(), 200);

    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger.len(), 1);
    assert!(ledger[0].cost_usd > 0.0);
    assert_eq!(ledger[0].prompt_id, 0);
}

#[tokio::test]
async fn audio_prompt_logging_disabled_still_bills() {
    // With `[storage] store_prompts = false` the policy skips the prompt
    // insert outright. Cost tracking is deliberately out of that scope, so
    // both audio endpoints must still write a ledger row.
    for path in ["/v1/audio/speech", "/v1/audio/transcriptions"] {
        let mock = common::mock_audio::MockAudioServer::start().await;
        let (server, db) = build_audio_app(
            mock.base_url(),
            AudioAppOpts { disable_prompt_storage: true, ..Default::default() },
        )
        .await;

        let req = server
            .post(path)
            .add_header(bearer("test-token").0, bearer("test-token").1);
        let resp = if path.ends_with("speech") {
            req.json(&serde_json::json!({"model": "tts-1", "input": "Hello", "voice": "alloy"}))
                .await
        } else {
            req.multipart(transcription_form()).await
        };
        assert_eq!(resp.status_code(), 200, "{path}");

        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        assert_eq!(ledger.len(), 1, "{path}");
        assert!(ledger[0].cost_usd > 0.0, "{path}");
        assert_eq!(ledger[0].prompt_id, 0, "{path}");
    }
}

#[tokio::test]
async fn audio_acquires_concurrency_permit_when_under_limit() {
    // A concurrency budget that is not yet exhausted takes the
    // permit-acquired branch of the policy check, the counterpart to the
    // limit-exceeded tests above.
    for path in ["/v1/audio/speech", "/v1/audio/transcriptions"] {
        let mock = common::mock_audio::MockAudioServer::start().await;
        let (server, db) = build_audio_app(
            mock.base_url(),
            AudioAppOpts { max_concurrent: Some(4), ..Default::default() },
        )
        .await;

        let req = server
            .post(path)
            .add_header(bearer("test-token").0, bearer("test-token").1);
        let resp = if path.ends_with("speech") {
            req.json(&serde_json::json!({"model": "tts-1", "input": "Hello", "voice": "alloy"}))
                .await
        } else {
            req.multipart(transcription_form()).await
        };
        assert_eq!(resp.status_code(), 200, "{path}");
        common::wait_for_ledger_rows(&*db, 1).await;
    }
}

#[tokio::test]
async fn speech_cost_scales_with_input_length() {
    // Speech is billed per character, so a longer input must cost strictly more.
    let (short_server, short_db, _m1) = test_app_with_mock_server().await;
    short_server
        .post("/v1/audio/speech")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({"model": "tts-1", "input": "hi", "voice": "alloy"}))
        .await;
    let short = common::wait_for_ledger_rows(&*short_db, 1).await;

    let (long_server, long_db, _m2) = test_app_with_mock_server().await;
    long_server
        .post("/v1/audio/speech")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({"model": "tts-1", "input": "hi".repeat(500), "voice": "alloy"}))
        .await;
    let long = common::wait_for_ledger_rows(&*long_db, 1).await;

    assert!(long[0].cost_usd > short[0].cost_usd);
    assert!(long[0].tokens_in > short[0].tokens_in);
}
