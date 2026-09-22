//! Azure AI Foundry provider (issue #27).
//!
//! Every test here is mocked HTTP. The development host has no route to Azure,
//! so the adapter is written against the published OpenAPI documents
//! (`specification/ai/data-plane/OpenAI.v1` and
//! `specification/ai/data-plane/ModelInference` in Azure/azure-rest-api-specs)
//! and these tests pin the parts of those documents that can be checked without
//! a live endpoint: the URL path per surface, the api-version rule per surface,
//! the request bodies, the credential flow and audience, and the parsing of
//! chat, streaming, embedding and catalog responses.

#![cfg(feature = "foundry")]

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use modelrouter::config::schema::ProviderConfig;
use modelrouter::providers::adapter::{NormalizedRequest, ProviderAdapter};
use modelrouter::providers::azure_entra::StaticTokenProvider;
use modelrouter::providers::catalog::ProviderCatalog;
use modelrouter::providers::catalog_registry::catalog_for;
use modelrouter::providers::embed_registry::EmbeddingRegistry;
use modelrouter::providers::embedding::{EmbeddingAdapter, EmbeddingRequest};
use modelrouter::providers::foundry::{FoundryAdapter, FoundryEmbeddingAdapter};
use modelrouter::providers::registry::ProviderRegistry;
use serde_json::json;
use serial_test::serial;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const AZURE_ENV_KEYS: &[&str] = &[
    "AZURE_TENANT_ID",
    "AZURE_CLIENT_ID",
    "AZURE_CLIENT_SECRET",
    "AZURE_AUTHORITY_HOST",
    "AZURE_POD_IDENTITY_AUTHORITY_HOST",
];

fn clear_azure_env() {
    for key in AZURE_ENV_KEYS {
        std::env::remove_var(key);
    }
}

// ── mock endpoint ───────────────────────────────────────────────────────────

#[derive(Default)]
struct Capture {
    paths: Vec<String>,
    queries: Vec<String>,
    bodies: Vec<serde_json::Value>,
    authorization: Vec<Option<String>>,
    api_key: Vec<Option<String>>,
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// A mock Foundry resource. Routes are the literal documented paths on BOTH
/// surfaces, so a path regression shows up as a 404 rather than passing.
async fn spawn_foundry(
    status: StatusCode,
    chat: serde_json::Value,
    embeddings: serde_json::Value,
    models: serde_json::Value,
    sse: Option<&'static str>,
) -> (String, Arc<Mutex<Capture>>) {
    let capture = Arc::new(Mutex::new(Capture::default()));

    fn record(capture: &Arc<Mutex<Capture>>, uri: &Uri, headers: &HeaderMap, body: &str) {
        let mut c = capture.lock().unwrap();
        c.paths.push(uri.path().to_string());
        c.queries.push(uri.query().unwrap_or("").to_string());
        c.bodies
            .push(serde_json::from_str(body).unwrap_or(serde_json::Value::Null));
        c.authorization.push(header(headers, "authorization"));
        c.api_key.push(header(headers, "api-key"));
    }

    let chat_handler = {
        let capture = capture.clone();
        move |uri: Uri, headers: HeaderMap, body: String| {
            let capture = capture.clone();
            let chat = chat.clone();
            async move {
                record(&capture, &uri, &headers, &body);
                let streaming = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["stream"].as_bool())
                    .unwrap_or(false);
                match (streaming, sse) {
                    (true, Some(events)) => (
                        status,
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        events,
                    )
                        .into_response(),
                    _ => (status, Json(chat)).into_response(),
                }
            }
        }
    };

    let embeddings_handler = {
        let capture = capture.clone();
        move |uri: Uri, headers: HeaderMap, body: String| {
            let capture = capture.clone();
            let embeddings = embeddings.clone();
            async move {
                record(&capture, &uri, &headers, &body);
                (status, Json(embeddings))
            }
        }
    };

    let models_handler = {
        let capture = capture.clone();
        move |uri: Uri, headers: HeaderMap| {
            let capture = capture.clone();
            let models = models.clone();
            async move {
                record(&capture, &uri, &headers, "");
                (status, Json(models))
            }
        }
    };

    let app = Router::new()
        // OpenAI-compatible surface.
        .route("/openai/v1/chat/completions", post(chat_handler.clone()))
        .route("/openai/v1/embeddings", post(embeddings_handler.clone()))
        .route("/openai/v1/models", get(models_handler))
        // Azure AI Model Inference surface.
        .route("/models/chat/completions", post(chat_handler))
        .route("/models/embeddings", post(embeddings_handler));

    (spawn(app).await, capture)
}

/// Mock Entra login authority (`{authority}/{tenant}/oauth2/v2.0/token`).
async fn spawn_login() -> (String, Arc<Mutex<Vec<String>>>) {
    let forms: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let state = forms.clone();
    let app = Router::new().route(
        "/:tenant/oauth2/v2.0/token",
        post(move |Path(_tenant): Path<String>, body: String| {
            let state = state.clone();
            async move {
                state.lock().unwrap().push(body);
                Json(json!({
                    "token_type": "Bearer",
                    "access_token": "mock-entra-token",
                    "expires_in": 3600,
                }))
            }
        }),
    );
    (spawn(app).await, forms)
}

/// Mock IMDS managed-identity endpoint.
async fn spawn_imds() -> (String, Arc<Mutex<Vec<String>>>) {
    let queries: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let state = queries.clone();
    let app = Router::new().route(
        "/metadata/identity/oauth2/token",
        get(move |headers: HeaderMap, uri: Uri| {
            let state = state.clone();
            async move {
                assert_eq!(header(&headers, "metadata").as_deref(), Some("true"));
                state.lock().unwrap().push(uri.query().unwrap_or("").to_string());
                Json(json!({
                    "token_type": "Bearer",
                    "access_token": "mock-imds-token",
                    "expires_in": "3599",
                }))
            }
        }),
    );
    (spawn(app).await, queries)
}

// ── fixtures ────────────────────────────────────────────────────────────────

const DEPLOYMENT: &str = "Llama-3.3-70B-Instruct";

fn config(endpoint: &str) -> ProviderConfig {
    ProviderConfig {
        foundry_endpoint: Some(endpoint.to_string()),
        timeout_secs: 30,
        ..Default::default()
    }
}

fn chat_payload() -> serde_json::Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": DEPLOYMENT,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "four"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 9,
            "completion_tokens": 1,
            "total_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 4}
        }
    })
}

fn embeddings_payload() -> serde_json::Value {
    json!({
        "object": "list",
        "model": "text-embedding-3-small",
        "data": [
            {"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]},
            {"object": "embedding", "index": 1, "embedding": [0.4, 0.5, 0.6]}
        ],
        "usage": {"prompt_tokens": 4, "total_tokens": 4}
    })
}

fn models_payload() -> serde_json::Value {
    json!({
        "object": "list",
        "data": [
            {"id": DEPLOYMENT, "object": "model", "owned_by": "meta"},
            {"id": "gpt-4o-mini", "object": "model", "owned_by": "openai"}
        ]
    })
}

const SSE_BODY: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"fo\"},\"index\":0}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"ur\"},\"index\":0,\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

async fn spawn_ok() -> (String, Arc<Mutex<Capture>>) {
    spawn_foundry(
        StatusCode::OK,
        chat_payload(),
        embeddings_payload(),
        models_payload(),
        Some(SSE_BODY),
    )
    .await
}

fn req(model: &str) -> NormalizedRequest {
    NormalizedRequest {
        model: model.to_string(),
        request_model: model.to_string(),
        messages: vec![json!({"role": "user", "content": "2+2?"})],
        stream: false,
        temperature: Some(0.1),
        max_tokens: Some(8),
        tools: None,
        tool_choice: None,
        extra_params: json!({}),
    }
}

fn static_adapter(config: &ProviderConfig) -> FoundryAdapter {
    FoundryAdapter::with_token_provider(config, Arc::new(StaticTokenProvider::new("static-token")))
        .unwrap()
}

// ── 1. chat round trip ──────────────────────────────────────────────────────

/// The default surface: bare host → `/openai/v1`, no api-version, deployment in
/// the body, Entra bearer token on the wire.
#[tokio::test]
async fn chat_round_trip_posts_to_the_openai_v1_surface_and_parses_usage() {
    let (base, capture) = spawn_ok().await;
    let adapter = static_adapter(&config(&base));

    let result = adapter.complete(&req(DEPLOYMENT)).await.unwrap();

    assert_eq!(result.content, "four");
    assert_eq!(result.prompt_tokens, 9);
    assert_eq!(result.completion_tokens, 1);
    assert_eq!(result.finish_reason, "stop");
    assert_eq!(result.cache_read_tokens, 4);
    assert!(result.ttft_ms.is_some(), "ttft is recorded at headers-received");

    let c = capture.lock().unwrap();
    assert_eq!(c.paths[0], "/openai/v1/chat/completions");
    // api-version is optional on this surface and defaults to `v1` server-side.
    assert_eq!(c.queries[0], "");
    assert_eq!(c.bodies[0]["model"], DEPLOYMENT);
    assert_eq!(c.bodies[0]["stream"], false);
    assert_eq!(c.bodies[0]["temperature"], 0.1);
    assert_eq!(c.bodies[0]["max_tokens"], 8);
    assert_eq!(c.authorization[0].as_deref(), Some("Bearer static-token"));
    assert!(c.api_key[0].is_none(), "Entra mode must not send api-key");
}

/// The Model Inference surface is selected by the configured path, and its
/// api-version — which the spec marks REQUIRED — is sent without the operator
/// having to know that.
#[tokio::test]
async fn the_model_inference_surface_sends_the_required_api_version() {
    let (base, capture) = spawn_ok().await;
    let adapter = static_adapter(&config(&format!("{base}/models")));

    adapter.complete(&req(DEPLOYMENT)).await.unwrap();

    let c = capture.lock().unwrap();
    assert_eq!(c.paths[0], "/models/chat/completions");
    assert!(
        c.queries[0].starts_with("api-version="),
        "required by the Model Inference spec: {}",
        c.queries[0]
    );
}

/// A 401 must name the audience actually requested and the knob that changes
/// it: the published spec and Microsoft's keyless-auth how-to disagree about
/// the audience for resource endpoints, so this is the first thing an operator
/// checks.
#[tokio::test]
async fn an_unauthorised_response_names_the_scope_and_the_override() {
    let (base, _) = spawn_foundry(
        StatusCode::UNAUTHORIZED,
        json!({"error": {"code": "PermissionDenied", "message": "AADSTS500011"}}),
        json!({}),
        json!({}),
        None,
    )
    .await;
    let adapter = static_adapter(&config(&base));

    let err = adapter.complete(&req(DEPLOYMENT)).await.unwrap_err().to_string();
    assert!(err.contains("401"), "{err}");
    assert!(err.contains("AADSTS500011"), "the Azure body must survive: {err}");
    assert!(err.contains("cognitiveservices.azure.com/.default"), "{err}");
    assert!(err.contains("entra_scope"), "{err}");
}

/// A 404 is nearly always a deployment-name typo; the message says so and names
/// the surface, because the other cause is pointing at the wrong one.
#[tokio::test]
async fn a_not_found_response_names_the_deployment_and_the_surface() {
    let (base, _) = spawn_foundry(
        StatusCode::NOT_FOUND,
        json!({"error": {"code": "DeploymentNotFound"}}),
        json!({}),
        json!({}),
        None,
    )
    .await;
    let adapter = static_adapter(&config(&base));

    let err = adapter.complete(&req("no-such-deployment")).await.unwrap_err().to_string();
    assert!(err.contains("no-such-deployment"), "{err}");
    assert!(err.contains("openai_v1"), "{err}");
}

// ── 2. streaming ────────────────────────────────────────────────────────────

/// SSE frames pass through byte-for-byte, `[DONE]` included, and `stream: true`
/// reaches the endpoint.
#[tokio::test]
async fn streaming_sets_the_flag_and_passes_sse_frames_through_untouched() {
    let (base, capture) = spawn_ok().await;
    let adapter = static_adapter(&config(&base));

    let mut stream = adapter.stream(&req(DEPLOYMENT)).await.unwrap();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk.unwrap());
    }
    let body = String::from_utf8(body).unwrap();

    assert_eq!(body, SSE_BODY, "frames must not be re-framed or rewritten");
    assert!(body.contains("data: [DONE]"));

    let c = capture.lock().unwrap();
    assert_eq!(c.paths[0], "/openai/v1/chat/completions");
    assert_eq!(c.bodies[0]["stream"], true);
    // The router owns usage capture (issue #84): the upstream body always
    // requests the final usage chunk, independent of the client's shape.
    assert_eq!(c.bodies[0]["stream_options"]["include_usage"], true);
}

/// A streaming call that fails must fail at `stream()`, not hand back a stream
/// that yields an error later — the route can still return a clean HTTP error
/// before any bytes are committed to the client.
#[tokio::test]
async fn a_streaming_failure_is_an_error_before_any_frame_is_emitted() {
    let (base, _) = spawn_foundry(
        StatusCode::TOO_MANY_REQUESTS,
        json!({"error": {"message": "rate limited"}}),
        json!({}),
        json!({}),
        None,
    )
    .await;
    let adapter = static_adapter(&config(&base));

    // `SseStream` is not Debug, so no `unwrap_err()` here.
    let err = match adapter.stream(&req(DEPLOYMENT)).await {
        Ok(_) => panic!("a 429 must fail before any frame is emitted"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("429"), "{err}");
    assert!(err.contains("rate limited"), "{err}");
}

// ── 3. embeddings ───────────────────────────────────────────────────────────

#[tokio::test]
async fn embeddings_round_trip_returns_vectors_in_order_with_prompt_tokens() {
    let (base, capture) = spawn_ok().await;
    let adapter = FoundryEmbeddingAdapter::with_token_provider(
        &config(&base),
        Arc::new(StaticTokenProvider::new("static-token")),
    )
    .unwrap();

    let result = adapter
        .embed(&EmbeddingRequest {
            model: "text-embedding-3-small".into(),
            input: vec!["alpha".into(), "beta".into()],
            dimensions: Some(3),
        })
        .await
        .unwrap();

    assert_eq!(result.embeddings.len(), 2);
    assert_eq!(result.embeddings[0].len(), 3);
    assert!((result.embeddings[1][2] - 0.6).abs() < 1e-6);
    assert_eq!(result.prompt_tokens, 4);

    let c = capture.lock().unwrap();
    assert_eq!(c.paths[0], "/openai/v1/embeddings");
    assert_eq!(c.bodies[0]["model"], "text-embedding-3-small");
    assert_eq!(c.bodies[0]["input"], json!(["alpha", "beta"]));
    // The whole batch goes in one call, and the pinned width is forwarded.
    assert_eq!(c.bodies[0]["dimensions"], 3);
    assert_eq!(c.authorization[0].as_deref(), Some("Bearer static-token"));
}

/// A deployment that ignores `dimensions` returns its native width. Storing
/// that silently corrupts every later similarity comparison, so it must fail.
#[tokio::test]
async fn an_embedding_of_the_wrong_width_is_refused() {
    let (base, _) = spawn_ok().await;
    let adapter = FoundryEmbeddingAdapter::with_token_provider(
        &config(&base),
        Arc::new(StaticTokenProvider::new("t")),
    )
    .unwrap();

    let err = adapter
        .embed(&EmbeddingRequest {
            model: "text-embedding-3-small".into(),
            input: vec!["alpha".into()],
            // The mock always returns 3-wide vectors.
            dimensions: Some(768),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("768"), "{err}");
    assert!(err.contains('3'), "{err}");
}

// ── 4. key auth is opt-in ───────────────────────────────────────────────────

/// Entra is the default and a key is a deliberate act: setting `api_key` swaps
/// the bearer token for the `api-key` header and skips Entra entirely (so this
/// test reads no environment variables at all).
#[tokio::test]
async fn api_key_opt_in_sends_the_api_key_header_instead_of_a_bearer_token() {
    let (base, capture) = spawn_ok().await;
    let mut c = config(&base);
    c.api_key = "resource-key-123".into();

    FoundryAdapter::new(&c).unwrap().complete(&req(DEPLOYMENT)).await.unwrap();

    let cap = capture.lock().unwrap();
    assert_eq!(cap.api_key[0].as_deref(), Some("resource-key-123"));
    assert!(cap.authorization[0].is_none());
}

/// The whole credential chain with nothing stubbed but the network:
/// client-credentials against the mock authority, then the inference call.
/// The audience for a resource endpoint is the cognitiveservices one, which is
/// what makes the shared token source scope-per-caller rather than fixed.
#[tokio::test]
#[serial]
async fn entra_client_credentials_request_the_cognitiveservices_audience() {
    clear_azure_env();
    let (login_base, forms) = spawn_login().await;
    let (base, capture) = spawn_ok().await;

    std::env::set_var("AZURE_TENANT_ID", "tenant-abc");
    std::env::set_var("AZURE_CLIENT_ID", "client-abc");
    std::env::set_var("AZURE_CLIENT_SECRET", "secret-abc");
    std::env::set_var("AZURE_AUTHORITY_HOST", &login_base);

    let adapter = FoundryAdapter::new(&config(&base)).unwrap();
    adapter.complete(&req(DEPLOYMENT)).await.unwrap();
    // A second call must reuse the cached token.
    adapter.complete(&req(DEPLOYMENT)).await.unwrap();
    clear_azure_env();

    let forms = forms.lock().unwrap();
    assert_eq!(forms.len(), 1, "token should be cached in-process");
    assert!(forms[0].contains("grant_type=client_credentials"), "{}", forms[0]);
    // Form-encoded: https%3A%2F%2Fcognitiveservices.azure.com%2F.default
    assert!(forms[0].contains("cognitiveservices.azure.com"), "{}", forms[0]);
    assert!(!forms[0].contains("ai.azure.com"), "{}", forms[0]);

    assert_eq!(
        capture.lock().unwrap().authorization[0].as_deref(),
        Some("Bearer mock-entra-token")
    );
}

/// No client secret in the environment → managed identity via IMDS, with the
/// caller's audience carried as the `resource` parameter (no `/.default`).
#[tokio::test]
#[serial]
async fn managed_identity_carries_the_callers_resource_to_imds() {
    clear_azure_env();
    let (imds_base, queries) = spawn_imds().await;
    let (base, capture) = spawn_ok().await;

    std::env::set_var("AZURE_POD_IDENTITY_AUTHORITY_HOST", &imds_base);

    FoundryAdapter::new(&config(&base))
        .unwrap()
        .complete(&req(DEPLOYMENT))
        .await
        .unwrap();
    clear_azure_env();

    let queries = queries.lock().unwrap();
    assert_eq!(queries.len(), 1);
    assert!(queries[0].contains("api-version=2018-02-01"), "{}", queries[0]);
    assert!(queries[0].contains("cognitiveservices.azure.com"), "{}", queries[0]);
    assert!(!queries[0].contains(".default"), "IMDS takes a resource: {}", queries[0]);

    assert_eq!(
        capture.lock().unwrap().authorization[0].as_deref(),
        Some("Bearer mock-imds-token")
    );
}

/// A project endpoint takes the other audience. Same token source, different
/// scope, chosen from the endpoint shape rather than hardcoded per provider.
#[tokio::test]
#[serial]
async fn a_project_endpoint_requests_the_ai_azure_audience_instead() {
    clear_azure_env();
    let (login_base, forms) = spawn_login().await;
    let (base, _) = spawn_ok().await;

    std::env::set_var("AZURE_TENANT_ID", "t");
    std::env::set_var("AZURE_CLIENT_ID", "c");
    std::env::set_var("AZURE_CLIENT_SECRET", "s");
    std::env::set_var("AZURE_AUTHORITY_HOST", &login_base);

    let mut c = config(&base);
    c.project = Some("my-project".into());
    // Construction alone does not mint a token; force one with a call. The
    // mock serves no `/api/projects/...` route, so the call itself 404s —
    // irrelevant here, the assertion is about which audience was requested.
    let adapter = FoundryAdapter::new(&c).unwrap();
    let _ = adapter.complete(&req(DEPLOYMENT)).await;
    clear_azure_env();

    let forms = forms.lock().unwrap();
    assert_eq!(forms.len(), 1);
    assert!(forms[0].contains("ai.azure.com"), "{}", forms[0]);
    assert!(!forms[0].contains("cognitiveservices"), "{}", forms[0]);
}

// ── 5. construction errors ──────────────────────────────────────────────────

#[test]
fn a_provider_table_without_an_endpoint_fails_construction_with_an_actionable_message() {
    let err = FoundryAdapter::new(&ProviderConfig::default())
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("foundry_endpoint"), "{err}");
    assert!(err.contains("[providers.foundry]"), "{err}");
    assert!(err.contains("services.ai.azure.com"), "{err}");

    // The embedding and catalog paths refuse for the same reason, so a
    // half-configured provider cannot half-work.
    assert!(FoundryEmbeddingAdapter::new(&ProviderConfig::default()).is_err());
    assert!(catalog_for("foundry", &ProviderConfig::default()).is_none());
}

/// `api_base` is accepted where `foundry_endpoint` is absent, so the provider
/// table can be written like every other one.
#[tokio::test]
async fn api_base_is_accepted_as_the_endpoint_fallback() {
    let (base, capture) = spawn_ok().await;
    let c = ProviderConfig {
        api_base: Some(base),
        ..Default::default()
    };

    static_adapter(&c).complete(&req(DEPLOYMENT)).await.unwrap();
    assert_eq!(capture.lock().unwrap().paths[0], "/openai/v1/chat/completions");
}

// ── 6. registry wiring and the feature guard ────────────────────────────────

/// Compiled in: both registries build a real Foundry adapter for the name.
/// Compiled out (`--no-default-features`) this whole file is skipped and the
/// guard is covered by `providers::feature_gate_tests` instead — the rule from
/// issue #24 being that "foundry" must never reach the OpenAI-compat arm.
#[tokio::test]
async fn the_registries_resolve_foundry_to_its_own_adapter() {
    let (base, capture) = spawn_ok().await;
    // Key auth, so this test asserts registry DISPATCH without depending on
    // ambient AZURE_* environment (the credential chain has its own tests).
    let mut cfg = config(&base);
    cfg.api_key = "k".into();
    let mut configs = HashMap::new();
    configs.insert("foundry".to_string(), cfg);

    ProviderRegistry::new(configs.clone())
        .get("foundry")
        .unwrap()
        .complete(&req(DEPLOYMENT))
        .await
        .unwrap();

    EmbeddingRegistry::new(configs)
        .get("foundry")
        .unwrap()
        .embed(&EmbeddingRequest {
            model: "text-embedding-3-small".into(),
            input: vec!["x".into()],
            dimensions: None,
        })
        .await
        .unwrap();

    let c = capture.lock().unwrap();
    // Both went to the Foundry paths, not to an OpenAI-compat `/chat/completions`
    // hung off the bare host.
    assert_eq!(c.paths[0], "/openai/v1/chat/completions");
    assert_eq!(c.paths[1], "/openai/v1/embeddings");
}

#[test]
fn provider_features_accept_a_configured_foundry_in_this_binary() {
    let mut configs = HashMap::new();
    configs.insert("foundry".to_string(), ProviderConfig::default());
    assert!(modelrouter::providers::validate_provider_features(&configs).is_ok());
}

// ── 7. catalog discovery ────────────────────────────────────────────────────

#[tokio::test]
async fn catalog_lists_deployments_as_routable_model_names() {
    let (base, capture) = spawn_ok().await;
    let models = static_adapter(&config(&base)).list_models().await.unwrap();

    let names: Vec<&str> = models.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, vec![DEPLOYMENT, "gpt-4o-mini"]);
    assert!(models.iter().all(|m| m.provider == "foundry"));

    let c = capture.lock().unwrap();
    assert_eq!(c.paths[0], "/openai/v1/models");
    assert_eq!(c.authorization[0].as_deref(), Some("Bearer static-token"));
}

/// The Model Inference surface has no listing operation — only a
/// per-deployment `/info`. Discovery therefore crosses to the sibling
/// OpenAI-compatible listing on the same resource rather than reporting no
/// catalog at all.
#[tokio::test]
async fn catalog_crosses_to_the_openai_v1_listing_from_the_model_inference_surface() {
    let (base, capture) = spawn_ok().await;
    let adapter = static_adapter(&config(&format!("{base}/models")));

    assert_eq!(adapter.list_models().await.unwrap().len(), 2);
    assert_eq!(capture.lock().unwrap().paths[0], "/openai/v1/models");
}

/// The aggregate catalog endpoint reaches the same implementation through the
/// registry, which is what feeds the mapping UI.
#[tokio::test]
async fn the_catalog_registry_routes_foundry_to_the_foundry_catalog() {
    let (base, _) = spawn_ok().await;
    let mut cfg = config(&base);
    cfg.api_key = "k".into(); // dispatch test, not a credential test
    let catalog = catalog_for("foundry", &cfg).expect("foundry has a catalog surface");
    let models = catalog.list_models().await.unwrap();
    assert_eq!(models[0].provider, "foundry");
    assert_eq!(models[0].name, DEPLOYMENT);
}

// ── 8. config plumbing ──────────────────────────────────────────────────────

/// The operator-facing keys must survive the real config loader, not just a
/// hand-built struct.
#[test]
#[serial]
fn config_toml_parses_the_foundry_provider_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
[providers.foundry]
foundry_endpoint = "https://res.services.ai.azure.com"
project          = "my-project"
entra_scope      = "https://ai.azure.com/.default"
api_version      = "2025-04-01"
timeout_secs     = 120

[routing]
default_provider = "foundry"

[routing.model_aliases]
fast = "foundry/Llama-3.3-70B-Instruct"
"#,
    )
    .unwrap();

    let settings = modelrouter::config::load(Some(path)).unwrap();
    let provider = settings.providers.get("foundry").unwrap();
    assert_eq!(
        provider.foundry_endpoint.as_deref(),
        Some("https://res.services.ai.azure.com")
    );
    assert_eq!(provider.project.as_deref(), Some("my-project"));
    assert_eq!(provider.entra_scope.as_deref(), Some("https://ai.azure.com/.default"));
    assert_eq!(provider.api_version.as_deref(), Some("2025-04-01"));
    assert_eq!(provider.timeout_secs, 120);
    // An alias mapping to `foundry/<deployment>` needs no special handling:
    // the router splits at the first `/`.
    assert_eq!(
        settings.routing.model_aliases.get("fast").map(String::as_str),
        Some("foundry/Llama-3.3-70B-Instruct")
    );
}

/// `ProviderConfig::default()` must describe the same provider as an empty
/// `[providers.foundry]` table, or code-built configs and file-built configs
/// diverge silently.
#[test]
fn provider_config_default_matches_an_empty_table_for_the_new_fields() {
    let from_toml: ProviderConfig = toml::from_str("").unwrap();
    let from_default = ProviderConfig::default();
    assert_eq!(from_default.foundry_endpoint, from_toml.foundry_endpoint);
    assert_eq!(from_default.entra_scope, from_toml.entra_scope);
    assert!(from_default.foundry_endpoint.is_none());
    assert!(from_default.entra_scope.is_none());
}
