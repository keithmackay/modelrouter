//! `GET /v1/audio/transcriptions/stream` end to end: the real route and a
//! fake provider session pinned into the transcription registry, driven by a
//! real WebSocket client on a loopback port. Nothing is mocked below the
//! WebSocket boundary; the provider is the one seam faked.
mod common;
#[path = "common/ws_client.rs"]
mod ws_client;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use common::transcription_app::{serve, start_message, AppOptions, TOKEN};
use ws_client::{drain_json, next_json, refused_status, send_audio, send_json, ClientSocket};
use modelrouter::config::schema::{ProviderConfig, TranscriptionConfig};
use modelrouter::db::models::CallKind;
use modelrouter::db::repositories::prompts::PromptRepository;
use modelrouter::providers::transcription::{
    AudioSink, StreamingTranscriptionAdapter, TranscriptEvent, TranscriptSegment, TranscriptionSessionParams,
    TranscriptionStream,
};
use serde_json::json;
use tokio::sync::mpsc;

const PROVIDER: &str = "fake";
const MODEL: &str = "transcribe-1";

/// What the fake provider does on `open` and after `finish`.
#[derive(Clone, Default)]
struct FakeBehaviour {
    /// Refuse to open with this message.
    refuse_open: Option<String>,
    /// After `finish`, never send `Ended` (exercises the drain bound).
    never_end: bool,
    /// Emit a session error after the first audio frame.
    error_on_audio: bool,
}

#[derive(Default)]
struct FakeRecord {
    params: Mutex<Vec<TranscriptionSessionParams>>,
    bytes: AtomicU64,
    finished: AtomicBool,
}

struct FakeAdapter {
    behaviour: FakeBehaviour,
    record: Arc<FakeRecord>,
}

struct FakeSink {
    behaviour: FakeBehaviour,
    record: Arc<FakeRecord>,
    events: mpsc::Sender<TranscriptEvent>,
    frames: u64,
}

fn segment(id: &str, text: &str, is_final: bool) -> TranscriptEvent {
    TranscriptEvent::Segment(TranscriptSegment {
        segment_id: id.to_string(),
        text: text.to_string(),
        is_final,
        start_ms: Some(0),
        end_ms: is_final.then_some(500),
        confidence: is_final.then_some(0.9),
        speaker: None,
    })
}

#[async_trait]
impl AudioSink for FakeSink {
    async fn send_audio(&mut self, frame: Bytes) -> anyhow::Result<()> {
        self.record.bytes.fetch_add(frame.len() as u64, Ordering::SeqCst);
        self.frames += 1;
        if self.behaviour.error_on_audio {
            let _ = self
                .events
                .send(TranscriptEvent::Error { code: "upstream_down".into(), message: "gone".into(), retryable: true })
                .await;
            return Ok(());
        }
        // One utterance per frame: a partial, then its final.
        let id = format!("u{}", self.frames);
        let _ = self.events.send(segment(&id, "hel", false)).await;
        let _ = self.events.send(segment(&id, "hello", true)).await;
        Ok(())
    }

    async fn finish(&mut self) -> anyhow::Result<()> {
        self.record.finished.store(true, Ordering::SeqCst);
        if self.behaviour.never_end {
            return Ok(());
        }
        let _ = self.events.send(segment("tail", "bye", true)).await;
        let _ = self.events.send(TranscriptEvent::Ended).await;
        Ok(())
    }
}

#[async_trait]
impl StreamingTranscriptionAdapter for FakeAdapter {
    async fn open(&self, params: &TranscriptionSessionParams) -> anyhow::Result<TranscriptionStream> {
        self.record.params.lock().unwrap().push(params.clone());
        if let Some(message) = &self.behaviour.refuse_open {
            anyhow::bail!("{message}");
        }
        let (tx, rx) = mpsc::channel(64);
        Ok(TranscriptionStream {
            audio: Box::new(FakeSink { behaviour: self.behaviour.clone(), record: self.record.clone(), events: tx, frames: 0 }),
            events: rx,
        })
    }
}

fn config(with_default_model: bool) -> TranscriptionConfig {
    TranscriptionConfig {
        provider: Some(PROVIDER.to_string()),
        model: with_default_model.then(|| MODEL.to_string()),
        usd_per_audio_minute: Some(0.60),
        ..Default::default()
    }
}

async fn app_with(transcription: TranscriptionConfig, behaviour: FakeBehaviour, deny_model: Option<&str>) -> (common::transcription_app::TranscriptionApp, Arc<FakeRecord>) {
    let record = Arc::new(FakeRecord::default());
    let adapter_record = record.clone();
    let app = serve(
        AppOptions { transcription, providers: HashMap::new(), deny_model: deny_model.map(str::to_string) },
        move |registry| registry.with_adapter(PROVIDER, FakeAdapter { behaviour, record: adapter_record }),
    )
    .await;
    (app, record)
}

/// One second of 16 kHz mono pcm16.
fn one_second() -> Vec<u8> {
    vec![0u8; 32_000]
}

#[tokio::test]
async fn a_session_streams_segments_and_is_metered_in_audio_seconds() {
    let (app, record) = app_with(config(true), FakeBehaviour::default(), None).await;
    let mut socket = ws_client::connect_with(&app, Some(TOKEN), &[("x-project", "proj-1")]).await.unwrap();

    send_json(&mut socket, start_message("call-1", None)).await;
    let started = next_json(&mut socket).await.unwrap();
    assert_eq!(started, json!({"type": "session.started", "session_id": "call-1", "provider": PROVIDER, "model": MODEL}));

    send_audio(&mut socket, one_second()).await;
    send_audio(&mut socket, one_second()).await;
    let partial = next_json(&mut socket).await.unwrap();
    assert_eq!(partial["type"], "transcript.segment");
    assert_eq!(partial["session_id"], "call-1");
    assert_eq!(partial["segment_id"], "u1");
    assert_eq!(partial["is_final"], false);
    assert_eq!(partial["text"], "hel");
    let final1 = next_json(&mut socket).await.unwrap();
    assert_eq!((final1["segment_id"].as_str(), final1["is_final"].as_bool(), final1["text"].as_str()), (Some("u1"), Some(true), Some("hello")));
    assert_eq!(final1["end_ms"], 500);
    assert_eq!(final1["confidence"], 0.9);
    assert!(final1["speaker"].is_null());
    let _partial2 = next_json(&mut socket).await.unwrap();
    let _final2 = next_json(&mut socket).await.unwrap();

    send_json(&mut socket, json!({"type": "session.end"})).await;
    let rest = drain_json(&mut socket).await;
    assert_eq!(rest.len(), 2, "{rest:?}");
    assert_eq!(rest[0]["segment_id"], "tail");
    assert_eq!(rest[1], json!({"type": "session.ended", "session_id": "call-1", "audio_ms": 2000, "segments": 5}));
    assert!(record.finished.load(Ordering::SeqCst));
    assert_eq!(record.bytes.load(Ordering::SeqCst), 64_000);

    let params = record.params.lock().unwrap();
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].session_id, "call-1");
    assert_eq!(params[0].model, MODEL);
    assert_eq!(params[0].audio.sample_rate_hz, 16_000);
    assert_eq!(params[0].language.as_deref(), Some("en"));
    drop(params);

    // Metered: 2 s of audio at $0.60/min = $0.02, under the attribution project.
    let rows = common::wait_for_ledger_rows(&*app.db, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, format!("{PROVIDER}/{MODEL}"));
    assert_eq!(rows[0].provider, PROVIDER);
    assert_eq!(rows[0].tokens_in, 2);
    assert!((rows[0].cost_usd - 0.02).abs() < 1e-9, "{}", rows[0].cost_usd);
    assert_eq!(rows[0].project.as_deref(), Some("proj-1"));
    assert_eq!(rows[0].call_kind, CallKind::Audio.as_str());
    assert!(!rows[0].tokens_estimated);
    let prompts = PromptRepository::list(&*app.db, 10, 0).await.unwrap();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].request_model, MODEL);
    assert_eq!(prompts[0].routed_model, format!("{PROVIDER}/{MODEL}"));
    // No transcript text is kept by the router: the row carries the storage
    // policy's placeholder (or an empty list), never a word that was said.
    assert!(!prompts[0].messages.contains("hello"), "{}", prompts[0].messages);
    assert!(prompts[0].response.is_none());
}

#[tokio::test]
async fn audio_append_text_frames_count_as_audio() {
    use base64::Engine as _;
    let (app, record) = app_with(config(true), FakeBehaviour::default(), None).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-2", None)).await;
    let _ = next_json(&mut socket).await;
    let encoded = base64::engine::general_purpose::STANDARD.encode(vec![1u8; 16_000]);
    send_json(&mut socket, json!({"type": "audio.append", "audio": encoded})).await;
    let _ = next_json(&mut socket).await;
    send_json(&mut socket, json!({"type": "session.end"})).await;
    let rest = drain_json(&mut socket).await;
    assert_eq!(rest.last().unwrap()["audio_ms"], 500);
    assert_eq!(record.bytes.load(Ordering::SeqCst), 16_000);
}

#[tokio::test]
async fn the_session_names_its_model_and_a_prefixed_model_resolves_to_the_provider() {
    let (app, record) = app_with(config(false), FakeBehaviour::default(), None).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-3", Some("fake/other-model"))).await;
    let started = next_json(&mut socket).await.unwrap();
    assert_eq!(started["model"], "other-model");
    send_json(&mut socket, json!({"type": "session.end"})).await;
    drain_json(&mut socket).await;
    assert_eq!(record.params.lock().unwrap()[0].model, "other-model");
    let rows = common::wait_for_ledger_rows(&*app.db, 1).await;
    assert_eq!(rows[0].model, "fake/other-model");
    // No audio: zero seconds, zero cost, still a row.
    assert_eq!(rows[0].tokens_in, 0);
}

#[tokio::test]
async fn without_a_key_the_upgrade_is_refused_with_401() {
    let (app, _) = app_with(config(true), FakeBehaviour::default(), None).await;
    let err = ws_client::connect_with(&app, None, &[]).await.unwrap_err();
    assert_eq!(refused_status(err), 401);
    let err = ws_client::connect_with(&app, Some("wrong"), &[]).await.unwrap_err();
    assert_eq!(refused_status(err), 401);
}

#[tokio::test]
async fn an_unconfigured_router_refuses_the_upgrade_with_503() {
    let app = serve(AppOptions::default(), |registry| registry).await;
    let err = ws_client::connect_with(&app, Some(TOKEN), &[]).await.unwrap_err();
    assert_eq!(refused_status(err), 503);
}

#[tokio::test]
async fn a_provider_that_cannot_stream_refuses_with_503() {
    let mut providers = HashMap::new();
    providers.insert("openai".to_string(), ProviderConfig::default());
    let app = serve(
        AppOptions {
            transcription: TranscriptionConfig { provider: Some("openai".into()), ..Default::default() },
            providers,
            deny_model: None,
        },
        |registry| registry,
    )
    .await;
    let err = ws_client::connect_with(&app, Some(TOKEN), &[]).await.unwrap_err();
    assert_eq!(refused_status(err), 503);
}

#[tokio::test]
async fn the_experiment_header_is_refused_with_400() {
    let (app, _) = app_with(config(true), FakeBehaviour::default(), None).await;
    let err = ws_client::connect_with(&app, Some(TOKEN), &[("x-modelrouter-experiment", "exp-1")]).await.unwrap_err();
    assert_eq!(refused_status(err), 400);
}

#[tokio::test]
async fn max_sessions_bounds_concurrent_sessions_with_429() {
    let mut cfg = config(true);
    cfg.max_sessions = 1;
    let (app, _) = app_with(cfg, FakeBehaviour::default(), None).await;
    let mut first = ws_client::connect(&app).await;
    send_json(&mut first, start_message("call-a", None)).await;
    let _ = next_json(&mut first).await;
    let err = ws_client::connect_with(&app, Some(TOKEN), &[]).await.unwrap_err();
    assert_eq!(refused_status(err), 429);
    // The slot is released when the session ends.
    send_json(&mut first, json!({"type": "session.end"})).await;
    drain_json(&mut first).await;
    for _ in 0..50 {
        if ws_client::connect_with(&app, Some(TOKEN), &[]).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the session slot was not released");
}

async fn first_error(socket: &mut ClientSocket) -> serde_json::Value {
    let message = next_json(socket).await.expect("an error frame before close");
    assert_eq!(message["type"], "error", "{message}");
    assert!(next_json(socket).await.is_none(), "the socket closes after an error");
    message
}

#[tokio::test]
async fn a_first_message_that_is_not_session_start_is_a_protocol_error() {
    let (app, _) = app_with(config(true), FakeBehaviour::default(), None).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, json!({"type": "session.end"})).await;
    let err = first_error(&mut socket).await;
    assert_eq!(err["code"], "protocol_error");
    assert_eq!(err["retryable"], false);

    let mut socket = ws_client::connect(&app).await;
    send_audio(&mut socket, one_second()).await;
    assert_eq!(first_error(&mut socket).await["code"], "protocol_error");

    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, json!({"type": "session.start", "session_id": "x", "audio": {"encoding": "pcm16", "sample_rate_hz": 1000, "channels": 1}})).await;
    let err = first_error(&mut socket).await;
    assert!(err["message"].as_str().unwrap().contains("sample_rate_hz"), "{err}");
}

#[tokio::test]
async fn no_session_start_within_the_start_timeout_is_a_protocol_error() {
    let mut cfg = config(true);
    cfg.start_timeout_secs = 1;
    let (app, _) = app_with(cfg, FakeBehaviour::default(), None).await;
    let mut socket = ws_client::connect(&app).await;
    let err = first_error(&mut socket).await;
    assert_eq!(err["code"], "protocol_error");
    assert!(err["message"].as_str().unwrap().contains("session.start"), "{err}");
}

#[tokio::test]
async fn a_model_for_another_provider_is_refused_and_no_model_at_all_is_named() {
    let (app, record) = app_with(config(false), FakeBehaviour::default(), None).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-4", Some("openai/whisper"))).await;
    let err = first_error(&mut socket).await;
    assert_eq!(err["code"], "model_not_served");

    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-5", None)).await;
    let err = first_error(&mut socket).await;
    assert_eq!(err["code"], "model_required");
    assert!(record.params.lock().unwrap().is_empty(), "no provider session was opened");
}

#[tokio::test]
async fn a_denied_model_is_refused_by_policy_before_the_provider_is_opened() {
    let (app, record) = app_with(config(true), FakeBehaviour::default(), Some(&format!("{PROVIDER}/{MODEL}"))).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-6", None)).await;
    let err = first_error(&mut socket).await;
    assert_eq!(err["code"], "policy_denied");
    assert!(record.params.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_provider_that_refuses_to_open_is_a_retryable_provider_error() {
    let (app, _) = app_with(config(true), FakeBehaviour { refuse_open: Some("handshake refused with 401".into()), ..Default::default() }, None).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-7", None)).await;
    let err = first_error(&mut socket).await;
    assert_eq!(err["code"], "provider_error");
    assert_eq!(err["retryable"], true);
    assert!(err["message"].as_str().unwrap().contains("handshake refused"), "{err}");
}

#[tokio::test]
async fn a_provider_error_mid_session_ends_it_with_the_providers_code() {
    let (app, _) = app_with(config(true), FakeBehaviour { error_on_audio: true, ..Default::default() }, None).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-8", None)).await;
    let _ = next_json(&mut socket).await;
    send_audio(&mut socket, one_second()).await;
    let err = first_error(&mut socket).await;
    assert_eq!(err["code"], "upstream_down");
    assert_eq!(err["retryable"], true);
    // The audio delivered before the failure is still metered.
    let rows = common::wait_for_ledger_rows(&*app.db, 1).await;
    assert_eq!(rows[0].tokens_in, 1);
}

#[tokio::test]
async fn an_idle_session_ends_with_idle_timeout() {
    let mut cfg = config(true);
    cfg.session_idle_timeout_secs = 1;
    let (app, _) = app_with(cfg, FakeBehaviour::default(), None).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-9", None)).await;
    let _ = next_json(&mut socket).await;
    let err = first_error(&mut socket).await;
    assert_eq!(err["code"], "idle_timeout");
}

#[tokio::test]
async fn a_provider_that_never_ends_is_bounded_by_the_drain_window() {
    let mut cfg = config(true);
    cfg.finish_drain_secs = 1;
    let (app, record) = app_with(cfg, FakeBehaviour { never_end: true, ..Default::default() }, None).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-10", None)).await;
    let _ = next_json(&mut socket).await;
    send_audio(&mut socket, one_second()).await;
    let _ = next_json(&mut socket).await;
    let _ = next_json(&mut socket).await;
    send_json(&mut socket, json!({"type": "session.end"})).await;
    let rest = drain_json(&mut socket).await;
    assert_eq!(rest.len(), 1, "{rest:?}");
    assert_eq!(rest[0]["type"], "session.ended");
    assert_eq!(rest[0]["segments"], 2);
    assert!(record.finished.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_caller_that_disconnects_still_finishes_the_provider_and_is_metered() {
    let (app, record) = app_with(config(true), FakeBehaviour::default(), None).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-11", None)).await;
    let _ = next_json(&mut socket).await;
    send_audio(&mut socket, one_second()).await;
    drop(socket);
    let rows = common::wait_for_ledger_rows(&*app.db, 1).await;
    assert_eq!(rows[0].tokens_in, 1);
    assert!(record.finished.load(Ordering::SeqCst));
}

#[tokio::test]
async fn an_unpriced_session_is_recorded_at_zero_and_flagged_estimated() {
    let mut cfg = config(true);
    cfg.usd_per_audio_minute = None;
    let (app, _) = app_with(cfg, FakeBehaviour::default(), None).await;
    let mut socket = ws_client::connect(&app).await;
    send_json(&mut socket, start_message("call-12", None)).await;
    let _ = next_json(&mut socket).await;
    send_audio(&mut socket, one_second()).await;
    send_json(&mut socket, json!({"type": "session.end"})).await;
    drain_json(&mut socket).await;
    let rows = common::wait_for_ledger_rows(&*app.db, 1).await;
    assert_eq!(rows[0].cost_usd, 0.0);
    assert!(rows[0].tokens_estimated);
}
