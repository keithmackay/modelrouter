//! The Foundry streaming transcription adapter end to end: the real route,
//! the real adapter, and a mock realtime WebSocket server on a loopback port
//! that records the handshake, the session configuration and the audio it was
//! sent, and answers with scripted realtime events. Nothing is mocked below
//! the WebSocket boundary on either side.
mod common;
#[path = "common/ws_client.rs"]
mod ws_client;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use base64::Engine as _;
use common::transcription_app::{serve, start_message, AppOptions};
use ws_client::{drain_json, next_json, send_audio, send_json};
use modelrouter::config::schema::{ProviderConfig, TranscriptionConfig};
use modelrouter::providers::azure_entra::StaticTokenProvider;
use modelrouter::providers::foundry::transcribe::FoundryTranscriptionAdapter;
use serde_json::{json, Value};

const DEPLOYMENT: &str = "transcribe-1";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    /// One utterance: started, two deltas, stopped, completed with logprobs.
    Happy,
    /// Refuse the upgrade with 401.
    RefuseHandshake,
    /// Answer the session configuration with a session-level error.
    SessionError,
}

#[derive(Default)]
struct Capture {
    headers: Vec<(String, String)>,
    query: HashMap<String, String>,
    messages: Vec<Value>,
    audio: Vec<u8>,
}

#[derive(Clone)]
struct MockState {
    script: Script,
    capture: Arc<Mutex<Capture>>,
}

async fn upgrade(
    ws: WebSocketUpgrade,
    State(state): State<MockState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    {
        let mut c = state.capture.lock().unwrap();
        c.query = query;
        for name in ["authorization", "api-key"] {
            if let Some(v) = headers.get(name) {
                c.headers.push((name.to_string(), v.to_str().unwrap().to_string()));
            }
        }
    }
    if state.script == Script::RefuseHandshake {
        return (StatusCode::UNAUTHORIZED, "credential rejected").into_response();
    }
    ws.on_upgrade(move |socket| realtime(socket, state))
}

async fn realtime(mut socket: WebSocket, state: MockState) {
    let say = |v: Value| Message::Text(v.to_string());
    let _ = socket.send(say(json!({"type": "session.created", "session": {"id": "sess_1"}}))).await;
    while let Some(Ok(message)) = socket.recv().await {
        let Message::Text(text) = message else { continue };
        let value: Value = serde_json::from_str(&text).unwrap();
        let kind = value["type"].as_str().unwrap_or("").to_string();
        let audio = value["audio"].as_str().map(str::to_string);
        state.capture.lock().unwrap().messages.push(value);
        match kind.as_str() {
            "session.update" | "transcription_session.update" => {
                if state.script == Script::SessionError {
                    let _ = socket
                        .send(say(json!({"type": "error", "error": {"type": "invalid_request_error", "code": "unknown_parameter", "message": "Unknown parameter: 'session.audio'."}})))
                        .await;
                    return;
                }
                let _ = socket.send(say(json!({"type": "session.updated"}))).await;
            }
            "input_audio_buffer.append" => {
                let bytes = base64::engine::general_purpose::STANDARD.decode(audio.unwrap()).unwrap();
                state.capture.lock().unwrap().audio.extend(bytes);
            }
            "input_audio_buffer.commit" => {
                let half = (0.5f64).ln();
                for event in [
                    json!({"type": "input_audio_buffer.committed", "item_id": "item_1"}),
                    json!({"type": "input_audio_buffer.speech_started", "item_id": "item_1", "audio_start_ms": 100}),
                    json!({"type": "conversation.item.input_audio_transcription.delta", "item_id": "item_1", "delta": "Good"}),
                    json!({"type": "conversation.item.input_audio_transcription.delta", "item_id": "item_1", "delta": " morning"}),
                    json!({"type": "input_audio_buffer.speech_stopped", "item_id": "item_1", "audio_end_ms": 900}),
                    json!({"type": "conversation.item.input_audio_transcription.completed", "item_id": "item_1", "transcript": "Good morning.", "logprobs": [{"token": "Good", "logprob": half}, {"token": " morning.", "logprob": half}]}),
                ] {
                    let _ = socket.send(say(event)).await;
                }
            }
            _ => {}
        }
    }
}

async fn spawn_mock(script: Script) -> (String, Arc<Mutex<Capture>>) {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let app = Router::new()
        .route("/openai/v1/realtime", get(upgrade))
        .with_state(MockState { script, capture: capture.clone() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), capture)
}

fn foundry_config(endpoint: &str, api_key: &str) -> ProviderConfig {
    ProviderConfig {
        foundry_endpoint: Some(endpoint.to_string()),
        api_key: api_key.to_string(),
        timeout_secs: 10,
        ..Default::default()
    }
}

fn transcription_config() -> TranscriptionConfig {
    TranscriptionConfig {
        provider: Some("foundry".to_string()),
        model: Some(DEPLOYMENT.to_string()),
        usd_per_audio_minute: Some(0.006),
        ..Default::default()
    }
}

/// The app with the real Foundry adapter pointed at `endpoint`, using an Entra
/// token source that returns a fixed token.
async fn app_with_entra(endpoint: &str, transcription: TranscriptionConfig) -> common::transcription_app::TranscriptionApp {
    let config = foundry_config(endpoint, "");
    let mut providers = HashMap::new();
    providers.insert("foundry".to_string(), config.clone());
    let adapter = FoundryTranscriptionAdapter::with_token_provider(
        &config,
        &transcription,
        Arc::new(StaticTokenProvider::new("static-token")),
    )
    .unwrap();
    serve(AppOptions { transcription, providers, deny_model: None }, move |registry| registry.with_adapter("foundry", adapter)).await
}

fn one_second_16k() -> Vec<u8> {
    // A ramp rather than silence, so the resampled length is what is checked
    // and not a run of zeros a bug could also produce.
    (0..16_000i16).flat_map(|i| (i % 2000).to_le_bytes()).collect()
}

#[tokio::test]
async fn audio_reaches_the_deployment_resampled_and_segments_come_back_mapped() {
    let (endpoint, capture) = spawn_mock(Script::Happy).await;
    let app = app_with_entra(&endpoint, transcription_config()).await;
    let mut socket = ws_client::connect(&app).await;

    send_json(&mut socket, start_message("call-f1", None)).await;
    let started = next_json(&mut socket).await.unwrap();
    assert_eq!(started, json!({"type": "session.started", "session_id": "call-f1", "provider": "foundry", "model": DEPLOYMENT}));

    send_audio(&mut socket, one_second_16k()).await;
    send_json(&mut socket, json!({"type": "session.end"})).await;
    let frames = drain_json(&mut socket).await;
    assert_eq!(frames.len(), 4, "{frames:?}");
    assert_eq!(frames[0]["type"], "transcript.segment");
    assert_eq!((frames[0]["segment_id"].as_str(), frames[0]["is_final"].as_bool(), frames[0]["text"].as_str()), (Some("item_1"), Some(false), Some("Good")));
    assert_eq!(frames[0]["start_ms"], 100);
    assert_eq!(frames[1]["text"], "Good morning");
    assert_eq!(frames[2]["is_final"], true);
    assert_eq!(frames[2]["text"], "Good morning.");
    assert_eq!((frames[2]["start_ms"].as_u64(), frames[2]["end_ms"].as_u64()), (Some(100), Some(900)));
    assert!((frames[2]["confidence"].as_f64().unwrap() - 0.5).abs() < 1e-9, "{}", frames[2]);
    assert!(frames[2]["speaker"].is_null());
    assert_eq!(frames[3], json!({"type": "session.ended", "session_id": "call-f1", "audio_ms": 1000, "segments": 3}));

    let c = capture.lock().unwrap();
    assert_eq!(c.query.get("intent").map(String::as_str), Some("transcription"));
    assert_eq!(c.headers, vec![("authorization".to_string(), "Bearer static-token".to_string())]);
    // The session configuration names the deployment and the language, GA shape.
    assert_eq!(c.messages[0]["type"], "session.update");
    assert_eq!(c.messages[0]["session"]["type"], "transcription");
    assert_eq!(c.messages[0]["session"]["audio"]["input"]["transcription"]["model"], DEPLOYMENT);
    assert_eq!(c.messages[0]["session"]["audio"]["input"]["transcription"]["language"], "en");
    assert_eq!(c.messages[0]["session"]["audio"]["input"]["format"]["rate"], 24000);
    // One second at 16 kHz became one second at 24 kHz: 48,000 bytes, give or
    // take the interpolation's last sample.
    assert!((47_990..=48_000).contains(&c.audio.len()), "{}", c.audio.len());
    assert!(c.audio.iter().any(|b| *b != 0), "the resampled audio is not silence");
    assert_eq!(c.messages.last().unwrap()["type"], "input_audio_buffer.commit");
    drop(c);

    let rows = common::wait_for_ledger_rows(&*app.db, 1).await;
    assert_eq!(rows[0].model, format!("foundry/{DEPLOYMENT}"));
    assert_eq!(rows[0].tokens_in, 1);
    assert!((rows[0].cost_usd - 0.0001).abs() < 1e-9, "{}", rows[0].cost_usd);
}

#[tokio::test]
async fn a_configured_api_key_is_sent_as_the_api_key_header() {
    let (endpoint, capture) = spawn_mock(Script::Happy).await;
    let config = foundry_config(&endpoint, "secret-key");
    let mut providers = HashMap::new();
    providers.insert("foundry".to_string(), config.clone());
    // Built through `new`, as production does: the non-empty key opts into
    // key auth, so no Entra credential is needed.
    let app = serve(AppOptions { transcription: transcription_config(), providers, deny_model: None }, |registry| registry).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-f2", None)).await;
    let started = next_json(&mut socket).await.unwrap();
    assert_eq!(started["type"], "session.started", "{started}");
    send_json(&mut socket, json!({"type": "session.end"})).await;
    drain_json(&mut socket).await;
    let c = capture.lock().unwrap();
    assert_eq!(c.headers, vec![("api-key".to_string(), "secret-key".to_string())]);
}

#[tokio::test]
async fn the_beta_session_shape_is_sent_when_configured() {
    let (endpoint, capture) = spawn_mock(Script::Happy).await;
    let mut transcription = transcription_config();
    transcription.session_shape = "beta".to_string();
    let app = app_with_entra(&endpoint, transcription).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-f3", None)).await;
    let _ = next_json(&mut socket).await;
    send_json(&mut socket, json!({"type": "session.end"})).await;
    drain_json(&mut socket).await;
    let c = capture.lock().unwrap();
    assert_eq!(c.messages[0]["type"], "transcription_session.update");
    assert_eq!(c.messages[0]["session"]["input_audio_transcription"]["model"], DEPLOYMENT);
}

#[tokio::test]
async fn a_refused_handshake_is_a_retryable_provider_error_naming_the_status() {
    let (endpoint, _) = spawn_mock(Script::RefuseHandshake).await;
    let app = app_with_entra(&endpoint, transcription_config()).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-f4", None)).await;
    let err = next_json(&mut socket).await.unwrap();
    assert_eq!(err["type"], "error");
    assert_eq!(err["code"], "provider_error");
    assert_eq!(err["retryable"], true);
    assert!(err["message"].as_str().unwrap().contains("401"), "{err}");
    assert!(next_json(&mut socket).await.is_none());
}

#[tokio::test]
async fn a_session_level_error_from_the_deployment_ends_the_session_with_its_code() {
    let (endpoint, _) = spawn_mock(Script::SessionError).await;
    let app = app_with_entra(&endpoint, transcription_config()).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-f5", None)).await;
    let started = next_json(&mut socket).await.unwrap();
    assert_eq!(started["type"], "session.started");
    let err = next_json(&mut socket).await.unwrap();
    assert_eq!(err["type"], "error", "{err}");
    assert_eq!(err["code"], "unknown_parameter");
    assert_eq!(err["retryable"], false);
    assert!(next_json(&mut socket).await.is_none());
}

#[tokio::test]
async fn a_configured_url_overrides_the_derived_one() {
    let (endpoint, _) = spawn_mock(Script::Happy).await;
    let config = foundry_config("https://unreachable.example", "");
    let mut transcription = transcription_config();
    transcription.url = Some(format!("{}/openai/v1/realtime?intent=transcription", endpoint.replace("http://", "ws://")));
    let adapter = FoundryTranscriptionAdapter::with_token_provider(&config, &transcription, Arc::new(StaticTokenProvider::new("t"))).unwrap();
    assert!(adapter.url().starts_with("ws://127.0.0.1:"), "{}", adapter.url());
    let derived = FoundryTranscriptionAdapter::with_token_provider(&config, &transcription_config(), Arc::new(StaticTokenProvider::new("t"))).unwrap();
    assert_eq!(derived.url(), "wss://unreachable.example/openai/v1/realtime?intent=transcription");
}
