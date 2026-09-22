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

use std::collections::HashMap;
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

/// One request the mock received, captured for later assertion, together with
/// what it served back.
///
/// Recording the served side is what lets a test compute the aggregates it
/// expects without restating them as literals: the mock is the only authority
/// on the token counts it invented, so a results assertion can be checked
/// against them rather than against a number copied by hand.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub path: String,
    pub authorization: Option<String>,
    pub body: Value,
    /// HTTP status served for this request.
    pub served_status: u16,
    /// Usage served back, when the response carried a `usage` block.
    pub served_usage: Option<(u32, u32)>,
}

impl RecordedRequest {
    /// Prompt tokens served, or 0 for an error response.
    pub fn prompt_tokens(&self) -> u32 {
        self.served_usage.map(|(p, _)| p).unwrap_or(0)
    }

    /// Completion tokens served, or 0 for an error response.
    pub fn completion_tokens(&self) -> u32 {
        self.served_usage.map(|(_, c)| c).unwrap_or(0)
    }

    /// Whether the mock answered this request successfully.
    pub fn ok(&self) -> bool {
        self.served_status == 200
    }
}

/// A per-model behaviour profile, so an experiment produces data with real
/// spread instead of one constant repeated N times.
///
/// The spread is deterministic, not random: each field varies by a jitter
/// derived from a hash of the model name and that model's own call index, so
/// a test sees the same sequence on every run and can predict none of it by
/// accident. Two models given different centres therefore yield a stable,
/// assertable *trend* — the point of running an experiment at all.
#[derive(Debug, Clone, Copy)]
pub struct ModelProfile {
    /// Prompt tokens as (centre, jitter): the served value lands in
    /// `centre - jitter ..= centre + jitter`.
    pub prompt_tokens: (u32, u32),
    /// Completion tokens as (centre, jitter).
    pub completion_tokens: (u32, u32),
    /// Think-time before responding, as (centre_ms, jitter_ms). Kept small;
    /// it is real wall-clock the router measures into `latency_ms`.
    pub latency_ms: (u64, u64),
    /// Every Nth call to this model fails with a 500. 0 disables failures.
    pub fail_every: u32,
}

impl Default for ModelProfile {
    fn default() -> Self {
        Self {
            prompt_tokens: (100, 0),
            completion_tokens: (50, 0),
            latency_ms: (0, 0),
            fail_every: 0,
        }
    }
}

/// A 64-bit mixer (splitmix64 finaliser). Used instead of an RNG so the
/// sequence depends only on (model, call index) and not on how the test's
/// requests happened to interleave.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Stable per-model seed, so two models never share a jitter sequence.
fn model_seed(model: &str) -> u64 {
    model.bytes().fold(0xCBF2_9CE4_8422_2325_u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01B3)
    })
}

/// `centre` perturbed by up to `jitter` either way, deterministically.
/// The result lands in `centre - jitter ..= centre + jitter`.
fn jittered(centre: u64, jitter: u64, seed: u64, index: u64) -> u64 {
    if jitter == 0 {
        return centre;
    }
    let draw = mix(seed ^ index.wrapping_mul(0x2545_F491_4F6C_DD1D)) % (jitter * 2 + 1);
    (centre + draw).saturating_sub(jitter)
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
    /// Responses returned in order; when empty the profile, then the default,
    /// decides. An explicitly queued response always wins, so a test that
    /// scripts one exact reply is never overridden by a profile.
    queued: Vec<MockResponse>,
    default: Option<MockResponse>,
    /// Per-model behaviour, keyed by the `model` field of the request.
    profiles: HashMap<String, ModelProfile>,
    /// How many calls each model has served, driving its jitter sequence.
    calls: HashMap<String, u64>,
}

impl MockState {
    /// Next response for `model` from its profile, plus the think-time to
    /// sleep before serving it. Advances that model's call index.
    fn from_profile(&mut self, model: &str) -> Option<(MockResponse, u64)> {
        let profile = *self.profiles.get(model)?;
        let index = self.calls.entry(model.to_string()).or_insert(0);
        *index += 1;
        let n = *index;
        let seed = model_seed(model);
        let think = jittered(profile.latency_ms.0, profile.latency_ms.1, seed ^ 0xF00D, n);

        if profile.fail_every > 0 && n % profile.fail_every as u64 == 0 {
            return Some((
                MockResponse::error(StatusCode::INTERNAL_SERVER_ERROR, "mock upstream failure"),
                think,
            ));
        }
        let prompt = jittered(profile.prompt_tokens.0 as u64, profile.prompt_tokens.1 as u64, seed, n) as u32;
        let completion =
            jittered(profile.completion_tokens.0 as u64, profile.completion_tokens.1 as u64, seed ^ 0xBEEF, n) as u32;
        Some((MockResponse::completion("mock response", prompt, completion), think))
    }
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

    /// Give `model` a behaviour profile: token spread, think-time and failure
    /// cadence. Applies to requests whose `model` field matches exactly, which
    /// is the concrete model the router resolved, not the alias the caller
    /// asked for.
    pub fn set_profile(&self, model: &str, profile: ModelProfile) {
        self.state.lock().unwrap().profiles.insert(model.to_string(), profile);
    }

    /// Every request the mock served for `model`, in order.
    pub fn served_for(&self, model: &str) -> Vec<RecordedRequest> {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r.model() == Some(model))
            .cloned()
            .collect()
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
    let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();

    // Pick the response under the lock, then release it: the profile's
    // think-time is a real await and a std Mutex must not be held across one.
    let (response, think_ms) = {
        let mut s = state.lock().unwrap();
        if !s.queued.is_empty() {
            (s.queued.remove(0), 0)
        } else if let Some(scripted) = s.from_profile(&model) {
            scripted
        } else {
            let fallback = s
                .default
                .clone()
                .unwrap_or_else(|| MockResponse::completion("mock response", 10, 5));
            (fallback, 0)
        }
    };

    if think_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(think_ms)).await;
    }

    // Record after serving is decided, so the row carries what went back.
    {
        let mut s = state.lock().unwrap();
        let usage = response.body.get("usage").map(|u| {
            (
                u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            )
        });
        s.requests.push(RecordedRequest {
            path: "/v1/chat/completions".to_string(),
            authorization: headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            body: body.clone(),
            served_status: response.status.as_u16(),
            served_usage: usage,
        });
    }

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
        served_status: 200,
        served_usage: None,
    });
    Json(json!({
        "object": "list",
        "data": [{ "id": "mock-model", "object": "model", "owned_by": "mock" }]
    }))
}
