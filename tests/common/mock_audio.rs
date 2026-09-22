//! Mock HTTP server for audio (TTS/transcription) and image generation endpoints.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct RecordedAudioRequest {
    pub path: String,
    pub body: Option<Value>,
    pub served_status: u16,
    /// Content-Type the caller sent, e.g. `multipart/form-data; boundary=...`.
    pub content_type: Option<String>,
    /// Raw request body — for multipart this is the encoded form, so tests can
    /// assert the parts the router reassembled actually reached the provider.
    pub raw_body: Option<Vec<u8>>,
}

#[derive(Default)]
struct MockState {
    requests: Vec<RecordedAudioRequest>,
    speech_response: Option<(StatusCode, Vec<u8>)>,
    transcription_response: Option<(StatusCode, Value)>,
    /// Non-JSON transcription body (status, content-type, body). Takes
    /// precedence over `transcription_response` when set — used to exercise
    /// text/verbatim response formats the handler cannot parse as JSON.
    transcription_raw: Option<(StatusCode, String, String)>,
    image_response: Option<(StatusCode, Value)>,
}

pub struct MockAudioServer {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
}

impl MockAudioServer {
    pub async fn start() -> Self {
        let state = Arc::new(Mutex::new(MockState {
            speech_response: Some((StatusCode::OK, vec![0u8; 100])),
            transcription_response: Some((
                StatusCode::OK,
                json!({"text": "mock transcription"}),
            )),
            image_response: Some((
                StatusCode::OK,
                json!({"data": [{"url": "https://mock.example/image.png"}]}),
            )),
            ..Default::default()
        }));

        let app = Router::new()
            .route("/v1/audio/speech", post(speech))
            .route("/v1/audio/transcriptions", post(transcriptions))
            .route("/v1/images/generations", post(image_generations))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock audio server binds");
        let addr = listener.local_addr().expect("mock audio has address");

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self { addr, state }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn set_speech_response(&self, status: StatusCode, data: Vec<u8>) {
        self.state.lock().unwrap().speech_response = Some((status, data));
    }

    pub fn set_transcription_response(&self, status: StatusCode, body: Value) {
        let mut s = self.state.lock().unwrap();
        s.transcription_response = Some((status, body));
        s.transcription_raw = None;
    }

    /// Serve a non-JSON transcription body, as OpenAI does for
    /// `response_format=text`/`srt`/`vtt`.
    pub fn set_transcription_raw(&self, status: StatusCode, content_type: &str, body: &str) {
        self.state.lock().unwrap().transcription_raw =
            Some((status, content_type.to_string(), body.to_string()));
    }

    pub fn set_image_response(&self, status: StatusCode, body: Value) {
        self.state.lock().unwrap().image_response = Some((status, body));
    }

    pub fn set_speech_error(&self, status: StatusCode) {
        self.state.lock().unwrap().speech_response = Some((status, vec![]));
    }

    pub fn set_transcription_error(&self, status: StatusCode, message: &str) {
        self.state.lock().unwrap().transcription_response = Some((
            status,
            json!({"error": {"message": message, "type": "mock_error"}}),
        ));
    }

    pub fn set_image_error(&self, status: StatusCode, message: &str) {
        self.state.lock().unwrap().image_response = Some((
            status,
            json!({"error": {"message": message, "type": "mock_error"}}),
        ));
    }

    pub fn requests(&self) -> Vec<RecordedAudioRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn clear_requests(&self) {
        self.state.lock().unwrap().requests.clear();
    }
}

async fn speech(
    State(state): State<Arc<Mutex<MockState>>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let (status, data) = {
        let mut s = state.lock().unwrap();
        s.requests.push(RecordedAudioRequest {
            path: "/v1/audio/speech".to_string(),
            body: Some(body.clone()),
            served_status: 200,
            content_type: Some("application/json".to_string()),
            raw_body: None,
        });
        s.speech_response.clone().unwrap_or((StatusCode::OK, vec![0u8; 100]))
    };

    (
        status,
        [("content-type", "audio/mpeg")],
        data,
    )
}

async fn transcriptions(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: axum::http::HeaderMap,
    raw: Bytes,
) -> Response {
    let (raw_response, status, body) = {
        let mut s = state.lock().unwrap();
        s.requests.push(RecordedAudioRequest {
            path: "/v1/audio/transcriptions".to_string(),
            body: None,
            served_status: 200,
            content_type: headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_string()),
            raw_body: Some(raw.to_vec()),
        });
        let raw_response = s.transcription_raw.clone();
        let (status, body) = s.transcription_response.clone().unwrap_or((
            StatusCode::OK,
            json!({"text": "mock transcription"}),
        ));
        (raw_response, status, body)
    };

    match raw_response {
        Some((status, content_type, text)) => {
            (status, [("content-type", content_type)], text).into_response()
        }
        None => (status, Json(body)).into_response(),
    }
}

async fn image_generations(
    State(state): State<Arc<Mutex<MockState>>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let (status, resp_body) = {
        let mut s = state.lock().unwrap();
        s.requests.push(RecordedAudioRequest {
            path: "/v1/images/generations".to_string(),
            body: Some(body.clone()),
            served_status: 200,
            content_type: Some("application/json".to_string()),
            raw_body: None,
        });
        s.image_response.clone().unwrap_or((
            StatusCode::OK,
            json!({"data": [{"url": "https://mock.example/image.png"}]}),
        ))
    };

    (status, Json(resp_body))
}
