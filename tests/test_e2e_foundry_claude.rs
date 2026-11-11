//! End to end: tier aliases mapped to Claude deployments on Azure AI Foundry,
//! through a real `modelrouter serve`, against a mock Foundry resource.
//!
//! Proves the model-capability verdicts apply to `foundry/<deployment>` the
//! way they do to `anthropic/...` and `vertex/anthropic/...`: the deployment
//! name is looked up in the capability tables, so a deployment named after its
//! model gets that model's temperature, thinking and effort rules.
//!
//! Run with: `cargo test --test test_e2e_foundry_claude -- --ignored`

#![cfg(feature = "foundry")]

mod common;

use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use common::e2e::{RouterOptions, RouterProcess};
use common::mock_llm::MockLlm;
use serde_json::json;
use std::sync::{Arc, Mutex};

const SSE: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"x\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"streamed\"}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

/// A mock Foundry Messages surface recording every body it receives.
async fn spawn_foundry() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let seen = bodies.clone();
    let app = Router::new().route(
        "/anthropic/v1/messages",
        post(move |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| {
            let seen = seen.clone();
            async move {
                assert_eq!(headers.get("x-api-key").unwrap(), "foundry-test-key");
                assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
                let stream = body["stream"].as_bool().unwrap_or(false);
                seen.lock().unwrap().push(body);
                if stream {
                    ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], SSE).into_response()
                } else {
                    Json(json!({
                        "id": "m", "type": "message", "role": "assistant", "model": "x",
                        "content": [{"type": "text", "text": "ok"}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 5, "output_tokens": 2}
                    }))
                    .into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), bodies)
}

async fn router_on_foundry() -> (RouterProcess, Arc<Mutex<Vec<serde_json::Value>>>, String, MockLlm) {
    // The default `mock` provider must exist for the fixture config; nothing
    // here addresses it.
    let unused = MockLlm::start().await;
    let (foundry, bodies) = spawn_foundry().await;
    let router = RouterProcess::start(RouterOptions::new(unused.base_url()).with_extra_toml(format!(
        r#"
[providers.foundry]
foundry_endpoint = "{foundry}"
api_key = "foundry-test-key"
anthropic_deployments = ["claude-opus-5-5", "claude-haiku-4-5"]

[routing.model_aliases]
deep = "foundry/claude-opus-5-5"
fast = "foundry/claude-haiku-4-5"
"#
    )))
    .await;
    let key = router.create_user_and_key("caller");
    (router, bodies, key, unused)
}

async fn complete(router: &RouterProcess, key: &str, body: serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", router.base_url()))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .expect("POST completions")
}

#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn capability_verdicts_apply_per_claude_deployment() {
    let (router, bodies, key, _mock) = router_on_foundry().await;
    let tools = json!([{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object", "properties": {}}}}]);

    for tier in ["deep", "fast"] {
        let resp = complete(&router, &key, json!({
            "model": tier,
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.3,
            "max_tokens": 32000,
            "reasoning_effort": "high",
            "tools": tools,
        }))
        .await;
        assert!(resp.status().is_success(), "{tier}: {} — {}", resp.status(), router.logs());
    }

    let bodies = bodies.lock().unwrap();
    let (deep, fast) = (&bodies[0], &bodies[1]);
    assert_eq!(deep["model"], "claude-opus-5-5");
    assert!(deep.get("temperature").is_none(), "Opus 5.5 rejects temperature: {deep}");
    assert_eq!(deep["output_config"]["effort"], "high", "Opus 5.5 takes effort: {deep}");
    assert_eq!(deep["max_tokens"], 32000);
    assert_eq!(deep["tools"][0]["name"], "lookup", "tools pass the gate and are translated");

    assert_eq!(fast["model"], "claude-haiku-4-5");
    assert_eq!(fast["temperature"], 0.3, "Haiku 4.5 keeps temperature: {fast}");
    assert!(fast.get("output_config").is_none(), "Haiku 4.5 takes no effort: {fast}");
    assert_eq!(fast["tools"][0]["name"], "lookup");
}

#[tokio::test]
#[ignore = "e2e: spawns the real binary"]
async fn a_tier_streams_through_the_messages_surface() {
    let (router, bodies, key, _mock) = router_on_foundry().await;
    let resp = complete(&router, &key, json!({
        "model": "deep",
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}],
    }))
    .await;
    assert!(resp.status().is_success(), "{}", router.logs());
    let text = resp.text().await.unwrap();
    assert!(text.contains("\"content\":\"streamed\""), "{text}");
    assert!(text.contains("[DONE]"), "{text}");
    assert_eq!(bodies.lock().unwrap().last().unwrap()["stream"], true);
}
