//! Mock HTTP server for Anthropic Messages API.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct RecordedAnthropicRequest {
    pub body: Value,
    pub served_status: u16,
}

#[derive(Default)]
struct MockState {
    requests: Vec<RecordedAnthropicRequest>,
    response: Option<(StatusCode, Value)>,
    streaming_response: Option<(StatusCode, String)>,
}

pub struct MockAnthropicServer {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
}

impl MockAnthropicServer {
    pub async fn start() -> Self {
        let state = Arc::new(Mutex::new(MockState {
            response: Some((
                StatusCode::OK,
                json!({
                    "id": "msg-mock",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "Hello from mock"}],
                    "model": "claude-mock",
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 5
                    }
                }),
            )),
            streaming_response: None,
            ..Default::default()
        }));

        let app = Router::new()
            .route("/v1/messages", post(messages))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock anthropic server binds");
        let addr = listener.local_addr().expect("mock anthropic has address");

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self { addr, state }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn set_response(&self, status: StatusCode, body: Value) {
        let mut s = self.state.lock().unwrap();
        s.response = Some((status, body));
        s.streaming_response = None;
    }

    pub fn set_streaming_response(&self, status: StatusCode, sse_data: String) {
        let mut s = self.state.lock().unwrap();
        s.streaming_response = Some((status, sse_data));
        s.response = None;
    }

    pub fn set_error(&self, status: StatusCode, message: &str) {
        self.state.lock().unwrap().response = Some((
            status,
            json!({
                "type": "error",
                "error": {
                    "type": "mock_error",
                    "message": message
                }
            }),
        ));
    }

    pub fn requests(&self) -> Vec<RecordedAnthropicRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn clear_requests(&self) {
        self.state.lock().unwrap().requests.clear();
    }
}

async fn messages(
    State(state): State<Arc<Mutex<MockState>>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let is_streaming = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let (status, response, stream_data) = {
        let mut s = state.lock().unwrap();
        s.requests.push(RecordedAnthropicRequest {
            body: body.clone(),
            served_status: 200,
        });

        if is_streaming {
            let (st, data) = s.streaming_response.clone().unwrap_or((
                StatusCode::OK,
                r#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}

event: content_block_delta
data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}

event: message_stop
data: {"type":"message_stop"}

"#.to_string(),
            ));
            (st, None, Some(data))
        } else {
            let (st, resp) = s.response.clone().unwrap_or((
                StatusCode::OK,
                json!({
                    "id": "msg-mock",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "Hello"}],
                    "model": "claude-mock",
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 10, "output_tokens": 5}
                }),
            ));
            (st, Some(resp), None)
        }
    };

    if let Some(data) = stream_data {
        return (
            status,
            [("content-type", "text/event-stream")],
            data,
        ).into_response();
    }

    (status, Json(response.unwrap())).into_response()
}
