//! Grounding with Bing Search (Azure AI Foundry) search adapter.
//!
//! Every test here is mocked HTTP. The development host has no route to Azure,
//! so the adapter is written against the published REST spec
//! (<https://learn.microsoft.com/azure/ai-foundry/agents/how-to/tools/bing-tools>)
//! and these tests pin the parts of that spec we can check without a live
//! endpoint: the URL path, the request body, the credential flow, and the
//! normalisation of `url_citation` annotations — including that citation URLs
//! come back byte-for-byte, which Microsoft's Use and Display Requirements
//! oblige callers to honour.

#![cfg(feature = "bing-grounding")]

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::{get, post};
use axum::{Json, Router};
use modelrouter::config::schema::ProviderConfig;
use modelrouter::providers::azure_entra::StaticTokenProvider;
use modelrouter::providers::bing_grounding::BingGroundingAdapter;
use modelrouter::providers::search::{SearchAdapter, SearchRequest};
use modelrouter::providers::search_registry::{is_supported_engine, SearchRegistry};
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

// ── mock servers ────────────────────────────────────────────────────────────

#[derive(Default)]
struct FoundryCapture {
    bodies: Vec<serde_json::Value>,
    authorization: Vec<Option<String>>,
    api_key: Vec<Option<String>>,
}

#[derive(Default)]
struct TokenCapture {
    hits: usize,
    tenants: Vec<String>,
    bodies: Vec<String>,
    metadata_header: Vec<Option<String>>,
    query: Vec<String>,
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

/// Mock Foundry project endpoint. The route is the literal documented path, so
/// a change to `RESPONSES_PATH` shows up as a 404 rather than passing silently.
async fn spawn_foundry(
    status: StatusCode,
    response: serde_json::Value,
) -> (String, Arc<Mutex<FoundryCapture>>) {
    let capture = Arc::new(Mutex::new(FoundryCapture::default()));
    let state = capture.clone();
    let app = Router::new().route(
        "/openai/v1/responses",
        post(move |headers: HeaderMap, body: String| {
            let state = state.clone();
            let response = response.clone();
            async move {
                {
                    let mut c = state.lock().unwrap();
                    c.bodies
                        .push(serde_json::from_str(&body).unwrap_or(serde_json::Value::Null));
                    c.authorization.push(header(&headers, "authorization"));
                    c.api_key.push(header(&headers, "api-key"));
                }
                (status, Json(response))
            }
        }),
    );
    (spawn(app).await, capture)
}

/// Mock Entra login authority (`{authority}/{tenant}/oauth2/v2.0/token`).
async fn spawn_login() -> (String, Arc<Mutex<TokenCapture>>) {
    let capture = Arc::new(Mutex::new(TokenCapture::default()));
    let state = capture.clone();
    let app = Router::new().route(
        "/:tenant/oauth2/v2.0/token",
        post(move |Path(tenant): Path<String>, body: String| {
            let state = state.clone();
            async move {
                {
                    let mut c = state.lock().unwrap();
                    c.hits += 1;
                    c.tenants.push(tenant);
                    c.bodies.push(body);
                }
                Json(json!({
                    "token_type": "Bearer",
                    "access_token": "mock-entra-token",
                    "expires_in": 3600,
                }))
            }
        }),
    );
    (spawn(app).await, capture)
}

/// Mock IMDS managed-identity endpoint. Note `expires_in` is a STRING here,
/// which is what IMDS actually returns.
async fn spawn_imds() -> (String, Arc<Mutex<TokenCapture>>) {
    let capture = Arc::new(Mutex::new(TokenCapture::default()));
    let state = capture.clone();
    let app = Router::new().route(
        "/metadata/identity/oauth2/token",
        get(move |headers: HeaderMap, uri: Uri| {
            let state = state.clone();
            async move {
                {
                    let mut c = state.lock().unwrap();
                    c.hits += 1;
                    c.metadata_header.push(header(&headers, "metadata"));
                    c.query.push(uri.query().unwrap_or("").to_string());
                }
                Json(json!({
                    "token_type": "Bearer",
                    "access_token": "mock-imds-token",
                    "expires_in": "3599",
                    "resource": "https://ai.azure.com",
                }))
            }
        }),
    );
    (spawn(app).await, capture)
}

// ── fixtures ────────────────────────────────────────────────────────────────

const CONNECTION_ID: &str =
    "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg/providers/\
     Microsoft.CognitiveServices/accounts/acct/projects/proj/connections/bing-conn";

/// A citation URL that is hostile to helpful normalisation: percent-encoded
/// apostrophe, tracking parameters, trailing slash-less path. It must survive
/// intact — the Use and Display Requirements say "the exact form provided".
const CITATION_URL: &str =
    "https://en.wikipedia.org/wiki/Euler%27s_identity?utm_source=bing&form=QBRE";

fn base_config(endpoint: &str) -> ProviderConfig {
    ProviderConfig {
        foundry_project_endpoint: Some(endpoint.to_string()),
        project_connection_id: Some(CONNECTION_ID.to_string()),
        search_model: Some("model-deployment-1".to_string()),
        timeout_secs: 300,
        ..Default::default()
    }
}

/// Text is 80 characters; the two annotations end at 38 and 80, so each
/// citation's snippet is the sentence it follows.
fn grounded_payload() -> serde_json::Value {
    json!({
        "id": "resp_0001",
        "object": "response",
        "status": "completed",
        "model": "model-deployment-1",
        "output": [
            {
                "type": "bing_grounding_call",
                "id": "call_0001",
                "status": "completed",
                "url": "https://www.bing.com/search?q=euler%27s+identity"
            },
            {
                "type": "message",
                "id": "msg_0001",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": "Euler's identity links five constants. It is called a beautiful result.",
                    "annotations": [
                        {
                            "type": "url_citation",
                            "url": CITATION_URL,
                            "title": "Euler's identity - Wikipedia",
                            "start_index": 0,
                            "end_index": 38
                        },
                        {
                            "type": "url_citation",
                            "url": "https://example.org/beauty",
                            "start_index": 39,
                            "end_index": 71
                        }
                    ]
                }]
            }
        ]
    })
}

fn req(query: &str, max_results: Option<u32>) -> SearchRequest {
    SearchRequest {
        query: query.to_string(),
        max_results,
    }
}

fn static_adapter(config: &ProviderConfig) -> BingGroundingAdapter {
    BingGroundingAdapter::with_token_provider(
        config,
        Arc::new(StaticTokenProvider::new("static-token")),
    )
    .unwrap()
}

// ── end-to-end: token acquisition + grounded search ─────────────────────────

/// The whole chain with nothing stubbed but the network: client-credentials
/// token request against the mock authority, then the Foundry call, then
/// normalisation. Citation URLs must come back verbatim.
#[tokio::test]
#[serial]
async fn entra_client_credentials_then_grounded_search_returns_verbatim_citations() {
    clear_azure_env();
    let (login_base, login) = spawn_login().await;
    let (foundry_base, foundry) = spawn_foundry(StatusCode::OK, grounded_payload()).await;

    std::env::set_var("AZURE_TENANT_ID", "tenant-abc");
    std::env::set_var("AZURE_CLIENT_ID", "client-abc");
    std::env::set_var("AZURE_CLIENT_SECRET", "secret-abc");
    std::env::set_var("AZURE_AUTHORITY_HOST", &login_base);

    let adapter = BingGroundingAdapter::new(&base_config(&foundry_base)).unwrap();
    let resp = adapter.search(&req("euler's identity", Some(5))).await.unwrap();
    clear_azure_env();

    assert_eq!(resp.engine, "bing_grounding");
    assert_eq!(resp.results.len(), 2);

    assert_eq!(resp.results[0].url, CITATION_URL, "citation URL must not be rewritten");
    assert_eq!(resp.results[0].title, "Euler's identity - Wikipedia");
    assert_eq!(resp.results[0].snippet, "Euler's identity links five constants.");
    assert!(resp.results[0].score.is_none());
    assert!(resp.results[0].published_date.is_none());

    // No title in the annotation → the URL stands in, so the item is still
    // identifiable rather than blank.
    assert_eq!(resp.results[1].url, "https://example.org/beauty");
    assert_eq!(resp.results[1].title, "https://example.org/beauty");
    assert_eq!(resp.results[1].snippet, "It is called a beautiful result.");

    // Token flow: one request, right tenant, right scope, right grant.
    let login = login.lock().unwrap();
    assert_eq!(login.hits, 1);
    assert_eq!(login.tenants[0], "tenant-abc");
    let form = &login.bodies[0];
    assert!(form.contains("grant_type=client_credentials"), "{form}");
    assert!(form.contains("client_id=client-abc"), "{form}");
    // Scope is form-encoded: https%3A%2F%2Fai.azure.com%2F.default
    assert!(form.contains("ai.azure.com"), "{form}");
    assert!(form.contains(".default"), "{form}");

    let foundry = foundry.lock().unwrap();
    assert_eq!(
        foundry.authorization[0].as_deref(),
        Some("Bearer mock-entra-token")
    );
}

/// A token is fetched once and reused: two searches, one login round trip.
#[tokio::test]
#[serial]
async fn entra_token_is_cached_across_searches() {
    clear_azure_env();
    let (login_base, login) = spawn_login().await;
    let (foundry_base, _) = spawn_foundry(StatusCode::OK, grounded_payload()).await;

    std::env::set_var("AZURE_TENANT_ID", "tenant-abc");
    std::env::set_var("AZURE_CLIENT_ID", "client-abc");
    std::env::set_var("AZURE_CLIENT_SECRET", "secret-abc");
    std::env::set_var("AZURE_AUTHORITY_HOST", &login_base);

    let adapter = BingGroundingAdapter::new(&base_config(&foundry_base)).unwrap();
    adapter.search(&req("first", None)).await.unwrap();
    adapter.search(&req("second", None)).await.unwrap();
    clear_azure_env();

    assert_eq!(login.lock().unwrap().hits, 1, "token should be cached in-process");
}

/// With no client secret in the environment, the adapter falls back to the
/// managed-identity path: IMDS, `Metadata: true`, the Foundry resource, and an
/// `expires_in` that arrives as a string.
#[tokio::test]
#[serial]
async fn managed_identity_is_used_when_no_client_secret_is_set() {
    clear_azure_env();
    let (imds_base, imds) = spawn_imds().await;
    let (foundry_base, foundry) = spawn_foundry(StatusCode::OK, grounded_payload()).await;

    std::env::set_var("AZURE_POD_IDENTITY_AUTHORITY_HOST", &imds_base);

    let adapter = BingGroundingAdapter::new(&base_config(&foundry_base)).unwrap();
    let resp = adapter.search(&req("anything", None)).await.unwrap();
    clear_azure_env();

    assert_eq!(resp.results.len(), 2);

    let imds = imds.lock().unwrap();
    assert_eq!(imds.hits, 1);
    assert_eq!(imds.metadata_header[0].as_deref(), Some("true"));
    assert!(imds.query[0].contains("api-version=2018-02-01"), "{}", imds.query[0]);
    assert!(imds.query[0].contains("ai.azure.com"), "{}", imds.query[0]);

    assert_eq!(
        foundry.lock().unwrap().authorization[0].as_deref(),
        Some("Bearer mock-imds-token")
    );
}

// ── request shape ───────────────────────────────────────────────────────────

#[tokio::test]
async fn request_body_matches_the_documented_bing_grounding_tool_shape() {
    let (foundry_base, foundry) = spawn_foundry(StatusCode::OK, grounded_payload()).await;
    let adapter = static_adapter(&base_config(&foundry_base));
    adapter.search(&req("who won yesterday", Some(7))).await.unwrap();

    let foundry = foundry.lock().unwrap();
    let body = &foundry.bodies[0];
    assert_eq!(body["model"], "model-deployment-1");
    assert_eq!(body["tool_choice"], "required");
    assert!(
        body["input"].as_str().unwrap().ends_with("who won yesterday"),
        "the caller's query must be the tail of the instruction: {}",
        body["input"]
    );

    let tool = &body["tools"][0];
    assert_eq!(tool["type"], "bing_grounding");
    let config = &tool["bing_grounding"]["search_configurations"][0];
    assert_eq!(config["project_connection_id"], CONNECTION_ID);
    assert_eq!(config["count"], 7);
    assert!(config.get("instance_name").is_none());

    assert_eq!(
        foundry.authorization[0].as_deref(),
        Some("Bearer static-token")
    );
    assert!(foundry.api_key[0].is_none(), "Entra mode must not send api-key");
}

#[tokio::test]
async fn custom_search_flag_emits_the_preview_tool_type_and_instance() {
    let (foundry_base, foundry) = spawn_foundry(StatusCode::OK, grounded_payload()).await;
    let mut config = base_config(&foundry_base);
    config.custom_search = true;
    config.custom_search_instance = Some("my-instance".into());

    let adapter = static_adapter(&config);
    adapter.search(&req("restricted query", None)).await.unwrap();

    let foundry = foundry.lock().unwrap();
    let tool = &foundry.bodies[0]["tools"][0];
    assert_eq!(tool["type"], "bing_custom_search_preview");
    let search_config = &tool["bing_custom_search_preview"]["search_configurations"][0];
    assert_eq!(search_config["project_connection_id"], CONNECTION_ID);
    assert_eq!(search_config["instance_name"], "my-instance");
    // No max_results on the request → no `count` invented for the tool.
    assert!(search_config.get("count").is_none());
}

/// Key auth is an explicit opt-in, not the default: setting `api_key` swaps the
/// bearer token for the `api-key` header and skips Entra entirely (no env vars
/// are read, so this test needs none).
#[tokio::test]
async fn api_key_opt_in_sends_the_api_key_header_instead_of_a_bearer_token() {
    let (foundry_base, foundry) = spawn_foundry(StatusCode::OK, grounded_payload()).await;
    let mut config = base_config(&foundry_base);
    config.api_key = "resource-key-123".into();

    let adapter = BingGroundingAdapter::new(&config).unwrap();
    adapter.search(&req("anything", None)).await.unwrap();

    let foundry = foundry.lock().unwrap();
    assert_eq!(foundry.api_key[0].as_deref(), Some("resource-key-123"));
    assert!(foundry.authorization[0].is_none());
}

#[test]
fn api_version_is_only_appended_when_configured() {
    let mut config = base_config("https://acct.services.ai.azure.com/api/projects/proj/");
    // Trailing slash on the endpoint must not produce a double slash.
    assert_eq!(
        static_adapter(&config).responses_url(),
        "https://acct.services.ai.azure.com/api/projects/proj/openai/v1/responses"
    );

    config.api_version = Some("2025-11-15-preview".into());
    assert_eq!(
        static_adapter(&config).responses_url(),
        "https://acct.services.ai.azure.com/api/projects/proj/openai/v1/responses\
         ?api-version=2025-11-15-preview"
    );
}

// ── failure paths ───────────────────────────────────────────────────────────

/// A Foundry 5xx must be an `Err` so the search route's fallback chain walks on
/// to the next engine instead of returning a successful empty result.
#[tokio::test]
async fn foundry_500_is_an_error_so_the_fallback_chain_engages() {
    let (foundry_base, _) = spawn_foundry(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error": {"message": "upstream exploded"}}),
    )
    .await;
    let adapter = static_adapter(&base_config(&foundry_base));

    let err = adapter.search(&req("anything", None)).await.unwrap_err().to_string();
    assert!(err.contains("500"), "{err}");
    assert!(err.contains("upstream exploded"), "{err}");
}

/// The failure this provider exists to avoid: the model answering from memory.
/// Prose with no citations is not a search result, and must not be returned as
/// one — nor may the error quote the ungrounded text.
#[tokio::test]
async fn a_response_without_citations_is_an_error_not_an_empty_result() {
    let payload = json!({
        "id": "resp_0002",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "I believe the answer is 42.",
                "annotations": []
            }]
        }]
    });
    let (foundry_base, _) = spawn_foundry(StatusCode::OK, payload).await;
    let adapter = static_adapter(&base_config(&foundry_base));

    let err = adapter.search(&req("anything", None)).await.unwrap_err().to_string();
    assert!(err.contains("no web citations"), "{err}");
    assert!(!err.contains("42"), "the error must not carry ungrounded prose: {err}");
}

#[tokio::test]
async fn duplicate_citation_urls_collapse_and_max_results_truncates() {
    let payload = json!({
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "One. Two. Three.",
                "annotations": [
                    {"type": "url_citation", "url": "https://a.example/1", "end_index": 4},
                    {"type": "url_citation", "url": "https://a.example/1", "end_index": 9},
                    {"type": "url_citation", "url": "https://b.example/2", "end_index": 16},
                    {"type": "file_citation", "file_id": "f-1"}
                ]
            }]
        }]
    });
    let (foundry_base, _) = spawn_foundry(StatusCode::OK, payload).await;
    let adapter = static_adapter(&base_config(&foundry_base));

    let all = adapter.search(&req("anything", None)).await.unwrap();
    assert_eq!(all.results.len(), 2, "duplicate URL collapses, file_citation ignored");

    let capped = adapter.search(&req("anything", Some(1))).await.unwrap();
    assert_eq!(capped.results.len(), 1);
}

#[test]
fn missing_required_config_fails_construction_with_an_actionable_message() {
    let full = base_config("https://acct.services.ai.azure.com/api/projects/proj");

    let mut no_endpoint = full.clone();
    no_endpoint.foundry_project_endpoint = None;
    let err = BingGroundingAdapter::new(&no_endpoint).err().unwrap().to_string();
    assert!(err.contains("foundry_project_endpoint"), "{err}");

    let mut no_connection = full.clone();
    no_connection.project_connection_id = None;
    let err = BingGroundingAdapter::new(&no_connection).err().unwrap().to_string();
    assert!(err.contains("project_connection_id"), "{err}");

    let mut no_model = full.clone();
    no_model.search_model = None;
    let err = BingGroundingAdapter::new(&no_model).err().unwrap().to_string();
    assert!(err.contains("search_model"), "{err}");

    let mut custom_without_instance = full;
    custom_without_instance.custom_search = true;
    let err = BingGroundingAdapter::new(&custom_without_instance)
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("custom_search_instance"), "{err}");
}

/// `api_base` is accepted where `foundry_project_endpoint` is absent, so the
/// provider table can be written like every other one.
#[test]
fn api_base_is_accepted_as_the_endpoint_fallback() {
    let mut config = base_config("");
    config.foundry_project_endpoint = None;
    config.api_base = Some("https://acct.services.ai.azure.com/api/projects/proj".into());
    assert_eq!(
        static_adapter(&config).responses_url(),
        "https://acct.services.ai.azure.com/api/projects/proj/openai/v1/responses"
    );
}

// ── registry and config wiring ──────────────────────────────────────────────

#[test]
fn registry_supports_and_constructs_bing_grounding() {
    assert!(is_supported_engine("bing_grounding"));

    let mut configs = HashMap::new();
    configs.insert(
        "bing_grounding".to_string(),
        base_config("https://acct.services.ai.azure.com/api/projects/proj"),
    );
    let registry = SearchRegistry::new(configs);

    assert_eq!(registry.configured_engines(), vec!["bing_grounding".to_string()]);
    assert!(registry.get("bing_grounding").is_ok());
}

/// The operator-facing keys must survive the real config loader (the `config`
/// crate), not just a hand-built struct.
#[test]
#[serial]
fn config_toml_parses_the_bing_grounding_provider_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
[providers.bing_grounding]
foundry_project_endpoint = "https://acct.services.ai.azure.com/api/projects/proj"
project_connection_id    = "conn-1"
search_model             = "model-deployment-1"
custom_search            = true
custom_search_instance   = "my-instance"
timeout_secs             = 300
"#,
    )
    .unwrap();

    let settings = modelrouter::config::load(Some(path)).unwrap();
    let provider = settings.providers.get("bing_grounding").unwrap();
    assert_eq!(
        provider.foundry_project_endpoint.as_deref(),
        Some("https://acct.services.ai.azure.com/api/projects/proj")
    );
    assert_eq!(provider.project_connection_id.as_deref(), Some("conn-1"));
    assert_eq!(provider.search_model.as_deref(), Some("model-deployment-1"));
    assert!(provider.custom_search);
    assert_eq!(provider.custom_search_instance.as_deref(), Some("my-instance"));
    assert_eq!(provider.timeout_secs, 300);
}

#[test]
fn provider_config_default_leaves_bing_fields_unset() {
    let from_toml: ProviderConfig = toml::from_str("").unwrap();
    let from_default = ProviderConfig::default();
    assert_eq!(
        from_default.foundry_project_endpoint,
        from_toml.foundry_project_endpoint
    );
    assert_eq!(from_default.project_connection_id, from_toml.project_connection_id);
    assert_eq!(from_default.custom_search, from_toml.custom_search);
    assert_eq!(
        from_default.custom_search_instance,
        from_toml.custom_search_instance
    );
    assert!(!from_default.custom_search);
}
