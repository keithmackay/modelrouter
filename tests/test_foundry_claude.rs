//! Claude deployments on Azure AI Foundry (`anthropic_deployments`).
//!
//! Foundry serves Claude over the Anthropic Messages API at
//! `{resource}/anthropic/v1/messages` with an Entra bearer token (audience
//! `https://ai.azure.com/.default`) or `x-api-key`, and `anthropic-version`.
//! Mocked HTTP: the mock serves the Messages path and the OpenAI-shaped chat
//! path side by side, so a request sent down the wrong surface is a 404.

#![cfg(feature = "foundry")]

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use modelrouter::config::schema::ProviderConfig;
use modelrouter::providers::adapter::{NormalizedRequest, ProviderAdapter, ReasoningControl};
use modelrouter::providers::azure_entra::StaticTokenProvider;
use modelrouter::providers::foundry::FoundryAdapter;
use serde_json::json;
use serial_test::serial;
use std::sync::{Arc, Mutex};

const CLAUDE: &str = "claude-opus-5-5";
const OTHER: &str = "gpt-4.1-mini";

#[derive(Default, Clone, Debug)]
struct Seen {
    path: String,
    authorization: Option<String>,
    x_api_key: Option<String>,
    api_key: Option<String>,
    anthropic_version: Option<String>,
    body: serde_json::Value,
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

const MESSAGE_SSE: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"four\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

/// A mock Foundry resource answering `message` on the Messages path (SSE when
/// the body asks to stream) and a plain completion on the OpenAI path.
async fn spawn_foundry(status: StatusCode, message: serde_json::Value) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let record = |seen: &Arc<Mutex<Vec<Seen>>>, uri: &Uri, headers: &HeaderMap, body: &str| {
        seen.lock().unwrap().push(Seen {
            path: uri.path().to_string(),
            authorization: header(headers, "authorization"),
            x_api_key: header(headers, "x-api-key"),
            api_key: header(headers, "api-key"),
            anthropic_version: header(headers, "anthropic-version"),
            body: serde_json::from_str(body).unwrap_or_default(),
        });
    };
    let messages = {
        let seen = seen.clone();
        move |uri: Uri, headers: HeaderMap, body: String| {
            let seen = seen.clone();
            let message = message.clone();
            async move {
                record(&seen, &uri, &headers, &body);
                let stream = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["stream"].as_bool())
                    .unwrap_or(false);
                if stream && status.is_success() {
                    (status, [(axum::http::header::CONTENT_TYPE, "text/event-stream")], MESSAGE_SSE)
                        .into_response()
                } else {
                    (status, Json(message)).into_response()
                }
            }
        }
    };
    let chat = {
        let seen = seen.clone();
        move |uri: Uri, headers: HeaderMap, body: String| {
            let seen = seen.clone();
            async move {
                record(&seen, &uri, &headers, &body);
                Json(json!({
                    "choices": [{"message": {"role": "assistant", "content": "openai"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                }))
            }
        }
    };
    let app = Router::new()
        .route("/anthropic/v1/messages", post(messages))
        .route("/openai/v1/chat/completions", post(chat));
    (spawn(app).await, seen)
}

fn text_message() -> serde_json::Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": CLAUDE,
        "content": [{"type": "text", "text": "four"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 12, "output_tokens": 3, "cache_read_input_tokens": 5,
                  "output_tokens_details": {"thinking_tokens": 2}}
    })
}

fn config(endpoint: &str) -> ProviderConfig {
    ProviderConfig {
        foundry_endpoint: Some(endpoint.to_string()),
        anthropic_deployments: vec![CLAUDE.to_string()],
        timeout_secs: 30,
        ..Default::default()
    }
}

fn static_adapter(config: &ProviderConfig) -> FoundryAdapter {
    FoundryAdapter::with_token_provider(config, Arc::new(StaticTokenProvider::new("static-token")))
        .unwrap()
}

fn req(model: &str) -> NormalizedRequest {
    NormalizedRequest {
        model: model.to_string(),
        request_model: "deep".to_string(),
        messages: vec![
            json!({"role": "system", "content": "be brief"}),
            json!({"role": "user", "content": "2+2?"}),
        ],
        stream: false,
        temperature: None,
        max_tokens: None,
        tools: None,
        tool_choice: None,
        reasoning: None,
        extra_params: json!({}),
    }
}

#[tokio::test]
async fn a_claude_deployment_goes_to_the_messages_surface_with_bearer_auth() {
    let (base, seen) = spawn_foundry(StatusCode::OK, text_message()).await;
    let adapter = static_adapter(&config(&base));

    let result = adapter.complete(&req(CLAUDE)).await.unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let s = &seen[0];
    assert_eq!(s.path, "/anthropic/v1/messages");
    assert_eq!(s.authorization.as_deref(), Some("Bearer static-token"));
    assert_eq!(s.anthropic_version.as_deref(), Some("2023-06-01"));
    assert_eq!(s.body["model"], CLAUDE, "the deployment name selects the deployment");
    assert_eq!(s.body["system"][0]["text"], "be brief", "system prompt moves out of messages");
    assert_eq!(s.body["system"][0]["cache_control"]["type"], "ephemeral", "prompt caching applies as on direct Anthropic");
    assert_eq!(s.body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(s.body["max_tokens"], 4096, "Anthropic requires max_tokens; the default is sent");
    assert_eq!(result.content, "four");
    assert_eq!(result.prompt_tokens, 12);
    assert_eq!(result.completion_tokens, 3);
    assert_eq!(result.cache_read_tokens, 5);
    assert_eq!(result.reasoning_tokens, Some(2));
    assert_eq!(result.finish_reason, "end_turn", "same passthrough as the direct anthropic adapter");
}

#[tokio::test]
async fn other_deployments_on_the_same_resource_stay_on_the_openai_surface() {
    let (base, seen) = spawn_foundry(StatusCode::OK, text_message()).await;
    let adapter = static_adapter(&config(&base));

    let result = adapter.complete(&req(OTHER)).await.unwrap();

    assert_eq!(result.content, "openai");
    assert_eq!(seen.lock().unwrap()[0].path, "/openai/v1/chat/completions");
}

#[tokio::test]
async fn tools_and_reasoning_are_translated_for_claude() {
    let message = json!({
        "id": "msg_2", "type": "message", "role": "assistant", "model": CLAUDE,
        "content": [{"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "acme"}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 20, "output_tokens": 7}
    });
    let (base, seen) = spawn_foundry(StatusCode::OK, message).await;
    let adapter = static_adapter(&config(&base));
    let mut r = req(CLAUDE);
    r.tools = Some(vec![json!({"type": "function", "function": {
        "name": "lookup", "description": "find", "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}
    }})]);
    r.tool_choice = Some(json!("required"));
    r.reasoning = Some(ReasoningControl { disable_thinking: false, effort: Some("high") });
    r.max_tokens = Some(64000);

    assert!(adapter.supports_tools(CLAUDE));
    assert!(!adapter.supports_tools(OTHER), "the OpenAI surface is not wired for tools");
    let settings = adapter.effective_settings(&r);
    assert_eq!(settings.max_tokens, Some(64000));
    assert_eq!(settings.reasoning, r.reasoning);

    let result = adapter.complete(&r).await.unwrap();

    let body = seen.lock().unwrap()[0].body.clone();
    assert_eq!(body["tools"][0]["name"], "lookup");
    assert_eq!(body["tools"][0]["input_schema"]["properties"]["q"]["type"], "string");
    assert_eq!(body["tool_choice"]["type"], "any");
    assert_eq!(body["output_config"]["effort"], "high");
    assert_eq!(body["max_tokens"], 64000);
    assert_eq!(result.finish_reason, "tool_calls");
    let calls = result.tool_calls.expect("tool_use becomes tool_calls");
    assert_eq!(calls[0]["function"]["name"], "lookup");
}

#[tokio::test]
async fn streaming_translates_anthropic_events_to_openai_chunks() {
    let (base, seen) = spawn_foundry(StatusCode::OK, text_message()).await;
    let adapter = static_adapter(&config(&base));
    let mut r = req(CLAUDE);
    r.stream = true;

    let mut stream = adapter.stream(&r).await.unwrap();
    let mut out = String::new();
    while let Some(chunk) = stream.next().await {
        out.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }

    assert_eq!(seen.lock().unwrap()[0].body["stream"], true);
    assert!(out.contains("chat.completion.chunk"), "{out}");
    assert!(out.contains("\"content\":\"four\""), "{out}");
    assert!(out.contains("[DONE]"), "{out}");
    assert!(!out.contains("message_start"), "Anthropic events must not leak through: {out}");
}

#[tokio::test]
async fn key_auth_uses_the_x_api_key_header_on_the_messages_surface() {
    let (base, seen) = spawn_foundry(StatusCode::OK, text_message()).await;
    let mut c = config(&base);
    c.api_key = "foundry-key".into();
    let adapter = FoundryAdapter::new(&c).unwrap();

    adapter.complete(&req(CLAUDE)).await.unwrap();

    let s = seen.lock().unwrap()[0].clone();
    assert_eq!(s.x_api_key.as_deref(), Some("foundry-key"));
    assert_eq!(s.api_key, None);
    assert_eq!(s.authorization, None);
}

#[tokio::test]
async fn errors_name_the_deployment_list_and_the_scope() {
    let (base, _) = spawn_foundry(StatusCode::NOT_FOUND, json!({"error": {"code": "DeploymentNotFound"}})).await;
    let adapter = static_adapter(&config(&base));
    let err = adapter.complete(&req(CLAUDE)).await.unwrap_err().to_string();
    assert!(err.contains("404") && err.contains("anthropic_deployments") && err.contains("DeploymentNotFound"), "{err}");

    let (base, _) = spawn_foundry(StatusCode::UNAUTHORIZED, json!({"error": "nope"})).await;
    let adapter = static_adapter(&config(&base));
    let err = adapter.complete(&req(CLAUDE)).await.unwrap_err().to_string();
    assert!(err.contains("https://ai.azure.com/.default") && err.contains("entra_scope"), "{err}");
}

/// Workload identity: the federated token is exchanged for an `ai.azure.com`
/// token for the Messages surface (Microsoft's documented audience), while the
/// OpenAI surface on a resource endpoint keeps `cognitiveservices`.
#[tokio::test]
#[serial]
async fn workload_identity_requests_the_ai_azure_audience_for_claude() {
    for key in ["AZURE_TENANT_ID", "AZURE_CLIENT_ID", "AZURE_CLIENT_SECRET", "AZURE_FEDERATED_TOKEN_FILE", "AZURE_AUTHORITY_HOST"] {
        std::env::remove_var(key);
    }
    let forms: Arc<Mutex<Vec<String>>> = Arc::default();
    let login = {
        let forms = forms.clone();
        Router::new().route(
            "/:tenant/oauth2/v2.0/token",
            post(move |Path(_t): Path<String>, body: String| {
                let forms = forms.clone();
                async move {
                    forms.lock().unwrap().push(body);
                    Json(json!({"token_type": "Bearer", "access_token": "wi-token", "expires_in": 3600}))
                }
            }),
        )
    };
    let login_base = spawn(login).await;
    let (base, seen) = spawn_foundry(StatusCode::OK, text_message()).await;
    std::env::set_var("AZURE_AUTHORITY_HOST", &login_base);
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, "federated-jwt").unwrap();

    let mut c = config(&base);
    c.credential_source = Some("workload-identity".into());
    c.azure_tenant_id = Some("tenant".into());
    c.azure_client_id = Some("client".into());
    c.azure_federated_token_file = Some(token_file.to_string_lossy().to_string());
    let adapter = FoundryAdapter::new(&c).unwrap();
    adapter.complete(&req(CLAUDE)).await.unwrap();
    adapter.complete(&req(OTHER)).await.unwrap();
    std::env::remove_var("AZURE_AUTHORITY_HOST");

    let forms = forms.lock().unwrap();
    assert!(forms.iter().any(|f| f.contains("client_assertion=federated-jwt") && f.contains("ai.azure.com")), "{forms:?}");
    assert!(forms.iter().any(|f| f.contains("cognitiveservices.azure.com")), "{forms:?}");
    assert!(seen.lock().unwrap().iter().all(|s| s.authorization.as_deref() == Some("Bearer wi-token")));
}

#[test]
fn config_toml_parses_anthropic_deployments_and_defaults_to_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[providers.foundry]\nfoundry_endpoint = \"https://res.services.ai.azure.com\"\n\
         anthropic_deployments = [\"claude-opus-5-5\", \"claude-haiku-4-5\"]\n",
    )
    .unwrap();
    let settings = modelrouter::config::load(Some(path)).unwrap();
    assert_eq!(
        settings.providers["foundry"].anthropic_deployments,
        vec!["claude-opus-5-5".to_string(), "claude-haiku-4-5".to_string()]
    );
    let from_toml: ProviderConfig = toml::from_str("").unwrap();
    assert!(from_toml.anthropic_deployments.is_empty());
    assert!(ProviderConfig::default().anthropic_deployments.is_empty());
}
