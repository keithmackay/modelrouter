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
    let adapter = AzureOpenAIAdapter::new(&config);
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
    let adapter = AzureOpenAIAdapter::new(&config);
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
    let adapter = AzureOpenAIAdapter::new(&config);
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
    let adapter = AzureOpenAIAdapter::new(&config);
    let req = NormalizedRequest {
        model: "gpt4o".into(),
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
    let adapter = OpenAICompatAdapter::new(&config);
    let req = NormalizedRequest {
        model: "gpt-4o".into(),
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
