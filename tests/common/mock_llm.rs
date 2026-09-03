//! A mock LLM provider that speaks the OpenAI-compatible shape over real HTTP.
//!
//! This exists so the end-to-end tier can exercise the *real* provider adapter
//! rather than substituting a trait impl. `ProviderRegistry` falls through to
//! `OpenAICompatAdapter` for any provider name it does not recognise, so a
//! config naming `mock` reaches this server through production code with no
//! test-only branch anywhere in `src/`.
//!
//! Two capabilities make assertions possible that an HTTP response alone cannot
//! support: the server records every request it received, so a test can ask what
//! modelrouter *sent upstream*; and its responses are programmable, so a test can
//! queue a 429 or a 500 and observe what the router does about it.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

/// One request the mock received, captured for later assertion.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub path: String,
    pub authorization: Option<String>,
    pub body: Value,
}

impl RecordedRequest {
    /// The `model` field the router asked the provider for.
    pub fn model(&self) -> Option<&str> {
        self.body.get("model").and_then(|m| m.as_str())
    }

    /// Whether the router asked for a streaming response.
    pub fn is_stream(&self) -> bool {
        self.body
            .get("stream")
            .and_then(|s| s.as_bool())
            .unwrap_or(false)
    }
}

/// A response the mock will return. Queue these to script a sequence.
#[derive(Debug, Clone)]
pub struct MockResponse {
    pub status: StatusCode,
    pub body: Value,
}

impl MockResponse {
    /// A normal completion carrying known token counts, so cost assertions have
    /// deterministic inputs.
    pub fn completion(content: &str, prompt_tokens: u32, completion_tokens: u32) -> Self {
        Self {
            status: StatusCode::OK,
            body: json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion",
                "created": 1_700_000_000,
                "model": "mock-model",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": content },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "total_tokens": prompt_tokens + completion_tokens
                }
            }),
        }
    }

    /// An upstream failure with the given status.
    pub fn error(status: StatusCode, message: &str) -> Self {
        Self {
            status,
            body: json!({ "error": { "message": message, "type": "mock_error" } }),
        }
    }
}

#[derive(Default)]
struct MockState {
    requests: Vec<RecordedRequest>,
    /// Responses returned in order; when empty the default is used.
    queued: Vec<MockResponse>,
    default: Option<MockResponse>,
}

/// A running mock provider. Dropping it leaves the server task to be reaped
/// with the test's runtime; the OS releases the ephemeral port.
pub struct MockLlm {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
}

impl MockLlm {
    /// Bind an ephemeral port and start serving. Never binds a fixed port, so
    /// concurrent tests cannot collide.
    pub async fn start() -> Self {
        let state = Arc::new(Mutex::new(MockState {
            default: Some(MockResponse::completion("mock response", 10, 5)),
            ..Default::default()
        }));

        let app = Router::new()
            .route("/v1/chat/completions", post(chat_completions))
            .route("/v1/models", get(list_models))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock provider binds an ephemeral port");
        let addr = listener.local_addr().expect("mock provider has an address");

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self { addr, state }
    }

    /// The `api_base` a provider config should point at.
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// Queue a response to be returned by the next request, ahead of the default.
    pub fn push_response(&self, response: MockResponse) {
        self.state.lock().unwrap().queued.push(response);
    }

    /// Replace the response returned when the queue is empty.
    pub fn set_default(&self, response: MockResponse) {
        self.state.lock().unwrap().default = Some(response);
    }

    /// Everything the mock has received so far.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    /// How many requests reached the provider. The assertion that a rejected
    /// request never reached upstream is this returning zero.
    pub fn request_count(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }

    /// Forget recorded requests, keeping queued responses.
    pub fn clear_requests(&self) {
        self.state.lock().unwrap().requests.clear();
    }
}

async fn chat_completions(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let response = {
        let mut s = state.lock().unwrap();
        s.requests.push(RecordedRequest {
            path: "/v1/chat/completions".to_string(),
            authorization: headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            body: body.clone(),
        });
        if s.queued.is_empty() {
            s.default
                .clone()
                .unwrap_or_else(|| MockResponse::completion("mock response", 10, 5))
        } else {
            s.queued.remove(0)
        }
    };

    // A streaming request gets a minimal but well-formed SSE body so the router's
    // streaming path can be exercised without pulling in a full SSE mock.
    if body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false)
        && response.status == StatusCode::OK
    {
        let chunk = json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000,
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "delta": { "content": "mock" },
                "finish_reason": null
            }]
        });
        let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        return (
            StatusCode::OK,
            [("content-type", "text/event-stream")],
            sse,
        )
            .into_response();
    }

    (response.status, Json(response.body)).into_response()
}

async fn list_models(State(state): State<Arc<Mutex<MockState>>>) -> impl IntoResponse {
    state.lock().unwrap().requests.push(RecordedRequest {
        path: "/v1/models".to_string(),
        authorization: None,
        body: Value::Null,
    });
    Json(json!({
        "object": "list",
        "data": [{ "id": "mock-model", "object": "model", "owned_by": "mock" }]
    }))
}
