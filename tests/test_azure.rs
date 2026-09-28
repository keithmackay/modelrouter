use modelrouter::config::schema::ProviderConfig;
use modelrouter::providers::azure_openai::AzureOpenAIAdapter;

#[test]
fn azure_adapter_builds_correct_url() {
    let config = ProviderConfig {
        api_key: "my-azure-key".to_string(),
        api_base: Some("https://my-resource.openai.azure.com/openai/deployments/my-gpt4".to_string()),
        api_version: Some("2024-02-01".to_string()),
        timeout_secs: 60,
        ..Default::default()
    };
    let adapter = AzureOpenAIAdapter::new(&config, modelrouter::config::schema::TierTimeoutsConfig::default());
    assert_eq!(
        adapter.chat_url(),
        "https://my-resource.openai.azure.com/openai/deployments/my-gpt4/chat/completions?api-version=2024-02-01"
    );
}

#[test]
fn azure_adapter_defaults_api_version() {
    let config = ProviderConfig {
        api_key: "key".to_string(),
        api_base: Some("https://resource.openai.azure.com/openai/deployments/gpt4".to_string()),
        api_version: None,
        timeout_secs: 60,
        ..Default::default()
    };
    let adapter = AzureOpenAIAdapter::new(&config, modelrouter::config::schema::TierTimeoutsConfig::default());
    assert!(adapter.chat_url().contains("api-version=2024-02-01"));
}

#[test]
fn azure_adapter_with_both_fields_set() {
    let config = ProviderConfig {
        api_key: "key".to_string(),
        api_base: Some("https://res.openai.azure.com/openai/deployments/gpt4o".to_string()),
        api_version: Some("2025-01-01".to_string()),
        timeout_secs: 30,
        ..Default::default()
    };
    let adapter = AzureOpenAIAdapter::new(&config, modelrouter::config::schema::TierTimeoutsConfig::default());
    let url = adapter.chat_url();
    assert!(url.starts_with("https://res.openai.azure.com"));
    assert!(url.ends_with("api-version=2025-01-01"));
}

/// The router owns usage capture (issue #84): streaming upstream bodies must
/// always carry `stream_options.include_usage`, and both OpenAI-shaped stream
/// adapters (Azure here, plus openai_compat below) are exercised against a
/// live localhost mock to prove the flag reaches the wire.
#[tokio::test]
async fn azure_stream_body_always_requests_usage() {
    use modelrouter::providers::adapter::{NormalizedRequest, ProviderAdapter};
    use std::sync::{Arc, Mutex};
    use futures::StreamExt;

    let seen: Arc<Mutex<serde_json::Value>> = Arc::new(Mutex::new(serde_json::Value::Null));
    let seen_handler = seen.clone();
    let router = axum::Router::new().fallback(axum::routing::post(move |body: String| {
        let seen = seen_handler.clone();
        async move {
            *seen.lock().unwrap() = serde_json::from_str(&body).unwrap();
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\ndata: [DONE]\n\n",
            )
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let config = ProviderConfig {
        api_key: "key".to_string(),
        api_base: Some(format!("http://{addr}/openai/deployments/gpt4o")),
        api_version: Some("2024-02-01".to_string()),
        timeout_secs: 30,
        ..Default::default()
    };
    let adapter = AzureOpenAIAdapter::new(&config, Default::default());
    let req = NormalizedRequest {
        model: "gpt4o".into(),
        request_model: "gpt4o".into(),
        messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
        stream: true,
        temperature: None,
        max_tokens: None,
        tools: None,
        tool_choice: None,
        extra_params: serde_json::json!({}),
    };
    let mut stream = adapter.stream(&req).await.unwrap();
    while let Some(c) = stream.next().await {
        c.unwrap();
    }

    let body = seen.lock().unwrap().clone();
    assert_eq!(body["stream"], true, "{body}");
    assert_eq!(body["stream_options"]["include_usage"], true, "{body}");
}

/// Same wire-level proof for the generic OpenAI-compatible adapter.
#[tokio::test]
async fn openai_compat_stream_body_always_requests_usage() {
    use modelrouter::providers::adapter::{NormalizedRequest, ProviderAdapter};
    use modelrouter::providers::openai_compat::OpenAICompatAdapter;
    use std::sync::{Arc, Mutex};
    use futures::StreamExt;

    let seen: Arc<Mutex<serde_json::Value>> = Arc::new(Mutex::new(serde_json::Value::Null));
    let seen_handler = seen.clone();
    let router = axum::Router::new().fallback(axum::routing::post(move |body: String| {
        let seen = seen_handler.clone();
        async move {
            *seen.lock().unwrap() = serde_json::from_str(&body).unwrap();
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\ndata: [DONE]\n\n",
            )
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let config = ProviderConfig {
        api_key: "key".to_string(),
        api_base: Some(format!("http://{addr}/v1")),
        timeout_secs: 30,
        ..Default::default()
    };
    let adapter = OpenAICompatAdapter::new(&config, Default::default());
    let req = NormalizedRequest {
        model: "gpt-4o".into(),
        request_model: "gpt-4o".into(),
        messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
        stream: true,
        temperature: None,
        max_tokens: None,
        tools: None,
        tool_choice: None,
        extra_params: serde_json::json!({}),
    };
    let mut stream = adapter.stream(&req).await.unwrap();
    while let Some(c) = stream.next().await {
        c.unwrap();
    }

    let body = seen.lock().unwrap().clone();
    assert_eq!(body["stream"], true, "{body}");
    assert_eq!(body["stream_options"]["include_usage"], true, "{body}");
}

/// With `credential_source` set, the `azure` provider sends a Microsoft Entra
/// bearer token and no `api-key` header; without it, key auth is unchanged.
#[tokio::test]
async fn azure_entra_mode_sends_a_bearer_token_and_no_key() {
    use modelrouter::providers::adapter::{NormalizedRequest, ProviderAdapter};
    use modelrouter::providers::azure_credentials::AzureAuth;
    use modelrouter::providers::azure_entra::StaticTokenProvider;
    use std::sync::{Arc, Mutex};

    /// (Authorization, api-key) per request.
    type Seen = Vec<(Option<String>, Option<String>)>;
    let seen: Arc<Mutex<Seen>> = Arc::new(Mutex::new(Vec::new()));
    let seen_handler = seen.clone();
    let router = axum::Router::new().fallback(move |headers: axum::http::HeaderMap| {
        let seen = seen_handler.clone();
        async move {
            let get = |k: &str| {
                headers
                    .get(k)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            seen.lock()
                .unwrap()
                .push((get("authorization"), get("api-key")));
            axum::Json(serde_json::json!({
                "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            }))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!(
        "http://{}/openai/deployments/d",
        listener.local_addr().unwrap()
    );
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let req = NormalizedRequest {
        model: "d".into(),
        request_model: "azure/d".into(),
        messages: vec![serde_json::json!({"role": "user", "content": "x"})],
        stream: false,
        temperature: None,
        max_tokens: None,
        tools: None,
        tool_choice: None,
        extra_params: serde_json::json!({}),
    };
    let config = ProviderConfig {
        api_base: Some(base.clone()),
        timeout_secs: 10,
        ..Default::default()
    };
    let tiers = modelrouter::config::schema::TierTimeoutsConfig::default();

    let entra = AzureOpenAIAdapter::with_auth(
        &config,
        tiers.clone(),
        AzureAuth::Entra(Arc::new(StaticTokenProvider::new("entra-token"))),
    );
    entra.complete(&req).await.unwrap();

    let keyed = AzureOpenAIAdapter::new(
        &ProviderConfig {
            api_key: "the-key".into(),
            ..config.clone()
        },
        tiers,
    );
    keyed.complete(&req).await.unwrap();
    assert!(keyed.credential_report().is_none());

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0], (Some("Bearer entra-token".to_string()), None));
    assert_eq!(seen[1], (None, Some("the-key".to_string())));
}

/// `credential_source` on `[providers.azure]` builds an Entra credential and
/// reports it; an invalid combination is an error, not a panic.
#[test]
fn azure_credential_source_selects_entra() {
    use modelrouter::providers::adapter::ProviderAdapter;
    let config = ProviderConfig {
        api_base: Some("https://example.invalid/openai/deployments/d".into()),
        credential_source: Some("workload-identity".into()),
        azure_tenant_id: Some("t".into()),
        azure_client_id: Some("c".into()),
        azure_federated_token_file: Some("/var/run/secrets/token".into()),
        ..Default::default()
    };
    let tiers = modelrouter::config::schema::TierTimeoutsConfig::default();
    let adapter = AzureOpenAIAdapter::try_new(&config, tiers.clone()).unwrap();
    let report = adapter.credential_report().unwrap();
    assert_eq!(report.provider, "azure");
    assert_eq!(report.source, "workload-identity");
    assert_eq!(report.kind, "azure-workload-identity");

    let contradictory = ProviderConfig {
        api_key: "k".into(),
        ..config
    };
    let err = AzureOpenAIAdapter::try_new(&contradictory, tiers)
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("contradictory"), "{err}");
}
