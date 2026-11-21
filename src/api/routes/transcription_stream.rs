//! `GET /v1/audio/transcriptions/stream`: streaming transcription over a
//! WebSocket. Audio frames in, transcript segments out, one provider session
//! per socket (`[transcription]`, `providers::transcription_registry`).
//!
//! ## Protocol
//!
//! The caller authenticates on the upgrade request like every other `/v1`
//! route (`Authorization: Bearer <api key>`; attribution headers as usual).
//! Then, caller to router:
//!
//! - `{"type":"session.start","session_id":"<id>","model":"<model>"?,
//!    "audio":{"encoding":"pcm16","sample_rate_hz":16000,"channels":1},
//!    "language":"en"?}` first, within `start_timeout_secs`;
//! - binary frames: raw audio in the declared format (or
//!   `{"type":"audio.append","audio":"<base64>"}` for a text-only client);
//! - `{"type":"session.end"}` when the conversation is over.
//!
//! Router to caller:
//!
//! - `{"type":"session.started","session_id","provider","model"}`;
//! - `{"type":"transcript.segment","segment_id","text","is_final","start_ms",
//!    "end_ms","confidence","speaker"}` as the provider recognises speech;
//!   a partial (`is_final: false`) carries the utterance's text so far and
//!   the final one for that `segment_id` replaces it;
//! - `{"type":"session.ended","session_id","audio_ms","segments"}` once the
//!   provider has delivered everything for the audio it was given, then a
//!   normal close;
//! - `{"type":"error","code","message","retryable"}` then a close, on a
//!   protocol mistake, a refused policy, an idle session or a provider
//!   failure.
//!
//! WebSocket pings every `heartbeat_secs` keep an idle socket open across
//! proxies. A session with no audio and no transcript for
//! `session_idle_timeout_secs` ends with `idle_timeout`; that bound is for a
//! dead caller, not a long conversation, which is why its default is hours.
//!
//! One usage row and one cost row are written per session, metered in audio
//! seconds; the router keeps no transcript text and logs none.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
    Extension,
};
use base64::Engine as _;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tracing::Instrument;

use crate::{
    api::{app::AppState, attribution::Attribution, auth::AuthenticatedUser, error::ApiError},
    db::models::{CallKind, NewCostLedgerEntry, NewPrompt, User},
    providers::{
        transcription::{AudioFormat, StreamingTranscriptionAdapter, TranscriptEvent, TranscriptSegment, TranscriptionSessionParams},
        transcription_registry::{TranscriptionRegistry, TranscriptionUnavailable},
    },
    router::policy::PolicyDecision,
};

pub const ROUTE: &str = "/v1/audio/transcriptions/stream";

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    #[serde(rename = "session.start")]
    SessionStart {
        session_id: String,
        #[serde(default)]
        model: Option<String>,
        audio: AudioFormat,
        #[serde(default)]
        language: Option<String>,
    },
    #[serde(rename = "audio.append")]
    AudioAppend { audio: String },
    #[serde(rename = "session.end")]
    SessionEnd,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "session.started")]
    SessionStarted { session_id: String, provider: String, model: String },
    #[serde(rename = "transcript.segment")]
    Segment {
        session_id: String,
        #[serde(flatten)]
        segment: TranscriptSegment,
    },
    #[serde(rename = "session.ended")]
    SessionEnded { session_id: String, audio_ms: u64, segments: u64 },
    #[serde(rename = "error")]
    Error { code: String, message: String, retryable: bool },
}

impl ServerMessage {
    fn error(code: &str, message: impl Into<String>, retryable: bool) -> Self {
        Self::Error { code: code.to_string(), message: message.into(), retryable }
    }
}

/// The upgrade handler: everything that can be refused before the socket
/// exists is refused here with a plain HTTP status, so a caller with the
/// wrong key or a router with no transcription provider never gets a socket
/// that immediately closes.
pub async fn transcription_stream(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Extension(registry): Extension<Arc<TranscriptionRegistry>>,
) -> Result<Response, ApiError> {
    crate::api::routes::reject_experiment_header(ROUTE, &headers)?;
    let user = user.0;
    let attribution = Attribution::from_headers(&headers)?;
    let provider = registry.provider_name().map_err(unavailable)?.to_string();
    let adapter = registry.adapter().map_err(unavailable)?;
    let admission = registry.admit().map_err(unavailable)?;
    let config = registry.config().clone();
    let span = tracing::info_span!("transcription_stream", user_id = user.id, provider = provider.as_str());
    Ok(ws.on_upgrade(move |socket| {
        async move {
            let session = Session { state, user, attribution, provider, adapter, config, _admission: admission };
            session.run(socket).await;
        }
        .instrument(span)
    }))
}

fn unavailable(err: TranscriptionUnavailable) -> ApiError {
    match err {
        TranscriptionUnavailable::Busy { .. } => ApiError::PolicyDenied { reason: err.to_string(), status: 429 },
        TranscriptionUnavailable::NotConfigured
        | TranscriptionUnavailable::ProviderMissing { .. }
        | TranscriptionUnavailable::Unsupported { .. }
        | TranscriptionUnavailable::Build { .. } => {
            let reason = format!("{err:#}");
            tracing::warn!(reason = reason.as_str(), "transcription_stream: refused before upgrade");
            ApiError::Refused {
                status: 503,
                body: serde_json::json!({
                    "error": {
                        "code": "transcription_not_configured",
                        "type": "transcription_not_configured",
                        "message": reason,
                    }
                }),
                reason,
            }
        }
    }
}

struct Session {
    state: AppState,
    user: User,
    attribution: Attribution,
    provider: String,
    adapter: Arc<dyn StreamingTranscriptionAdapter>,
    config: crate::config::schema::TranscriptionConfig,
    _admission: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// Why the session loop stopped, for the log line and the cost row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Ended,
    Error,
}

impl Session {
    async fn run(self, mut socket: WebSocket) {
        let start = match self.await_start(&mut socket).await {
            Ok(start) => start,
            Err(message) => {
                send(&mut socket, &message).await;
                let _ = socket.close().await;
                return;
            }
        };
        let session_id = start.session_id.clone();
        let (requested_model, model) = match self.resolve_model(start.model.as_deref()) {
            Ok(m) => m,
            Err(message) => {
                send(&mut socket, &message).await;
                let _ = socket.close().await;
                return;
            }
        };
        let routed_model = format!("{}/{}", self.provider, model);

        let _concurrency_permit = match self.check_policy(&routed_model).await {
            Ok(permit) => permit,
            Err(message) => {
                send(&mut socket, &message).await;
                let _ = socket.close().await;
                return;
            }
        };

        let params = TranscriptionSessionParams {
            session_id: session_id.clone(),
            model: model.clone(),
            audio: start.audio,
            language: start.language.clone(),
        };
        let stream = match self.adapter.open(&params).await {
            Ok(stream) => stream,
            Err(e) => {
                self.state.circuit_breaker.record_failure(&self.provider);
                tracing::warn!(
                    session_id = session_id.as_str(),
                    model = model.as_str(),
                    error = format!("{e:#}"),
                    "transcription_stream: provider session could not be opened"
                );
                send(
                    &mut socket,
                    &ServerMessage::error("provider_error", format!("the transcription provider refused the session: {e:#}"), true),
                )
                .await;
                let _ = socket.close().await;
                return;
            }
        };
        self.state.circuit_breaker.record_success(&self.provider);
        send(
            &mut socket,
            &ServerMessage::SessionStarted {
                session_id: session_id.clone(),
                provider: self.provider.clone(),
                model: model.clone(),
            },
        )
        .await;
        tracing::info!(
            session_id = session_id.as_str(),
            model = model.as_str(),
            sample_rate_hz = start.audio.sample_rate_hz,
            channels = start.audio.channels,
            "transcription_stream: session started"
        );

        let opened_at = Instant::now();
        let (outcome, audio_bytes, segments) = self.pump(&mut socket, stream, &session_id, start.audio).await;
        let audio_ms = start.audio.duration_ms(audio_bytes);
        tracing::info!(
            session_id = session_id.as_str(),
            outcome = ?outcome,
            audio_ms,
            segments,
            wall_ms = opened_at.elapsed().as_millis() as u64,
            "transcription_stream: session finished"
        );
        let _ = socket.close().await;
        self.record_usage(requested_model, routed_model, audio_ms);
    }

    /// The first message must be `session.start`, within `start_timeout_secs`.
    async fn await_start(&self, socket: &mut WebSocket) -> Result<StartParams, ServerMessage> {
        let deadline = Duration::from_secs(self.config.start_timeout_secs.max(1));
        let first = match tokio::time::timeout(deadline, socket.recv()).await {
            Err(_) => {
                return Err(ServerMessage::error(
                    "protocol_error",
                    format!("no session.start within {}s of the socket opening", deadline.as_secs()),
                    false,
                ))
            }
            Ok(None) | Ok(Some(Ok(Message::Close(_)))) => {
                return Err(ServerMessage::error("protocol_error", "the socket closed before session.start", false))
            }
            Ok(Some(Err(e))) => return Err(ServerMessage::error("protocol_error", format!("socket error before session.start: {e}"), false)),
            Ok(Some(Ok(message))) => message,
        };
        let text = match first {
            Message::Text(text) => text,
            _ => return Err(ServerMessage::error("protocol_error", "the first message must be a session.start text frame", false)),
        };
        match serde_json::from_str::<ClientMessage>(&text) {
            Ok(ClientMessage::SessionStart { session_id, model, audio, language }) => {
                validate_audio(&audio)?;
                if session_id.trim().is_empty() {
                    return Err(ServerMessage::error("protocol_error", "session.start needs a non-empty session_id", false));
                }
                Ok(StartParams { session_id, model, audio, language })
            }
            Ok(_) => Err(ServerMessage::error("protocol_error", "the first message must be session.start", false)),
            Err(e) => Err(ServerMessage::error("protocol_error", format!("session.start could not be parsed: {e}"), false)),
        }
    }

    /// `(requested, model)`: the model the caller named (or the configured
    /// default) and the bare deployment name for the configured provider. A
    /// `provider/` prefix naming another provider is refused: a session is
    /// one socket to the configured backend and cannot be routed elsewhere.
    fn resolve_model(&self, requested: Option<&str>) -> Result<(String, String), ServerMessage> {
        let requested = requested
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .or(self.config.model.as_deref().map(str::trim).filter(|m| !m.is_empty()))
            .ok_or_else(|| {
                ServerMessage::error(
                    "model_required",
                    "session.start names no model and [transcription] model sets no default",
                    false,
                )
            })?
            .to_string();
        let model = match requested.split_once('/') {
            Some((provider, model)) if provider == self.provider => model.to_string(),
            Some((provider, _)) => {
                return Err(ServerMessage::error(
                    "model_not_served",
                    format!(
                        "model {requested:?} names provider {provider:?}, but streaming transcription is served by {:?}",
                        self.provider
                    ),
                    false,
                ))
            }
            None => requested.clone(),
        };
        Ok((requested, model))
    }

    async fn check_policy(
        &self,
        routed_model: &str,
    ) -> Result<Option<crate::router::concurrency::ConcurrencyPermit>, ServerMessage> {
        let decision = self
            .state
            .policy
            .check(&self.user, routed_model)
            .instrument(tracing::info_span!("modelrouter.policy_check"))
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "transcription_stream: policy check failed");
                ServerMessage::error("internal_error", "policy check failed", true)
            })?;
        match decision {
            PolicyDecision::Allow { max_concurrent } => match max_concurrent {
                Some(max) => match self.state.concurrency.try_acquire(self.user.id, max).await {
                    Some(permit) => Ok(Some(permit)),
                    None => Err(ServerMessage::error("policy_denied", "concurrent request limit exceeded", true)),
                },
                None => Ok(None),
            },
            PolicyDecision::Deny { reason, .. } => Err(ServerMessage::error("policy_denied", reason, false)),
        }
    }

    /// The session loop. Returns the outcome, the audio bytes received and
    /// the segments forwarded.
    async fn pump(
        &self,
        socket: &mut WebSocket,
        mut stream: crate::providers::transcription::TranscriptionStream,
        session_id: &str,
        audio: AudioFormat,
    ) -> (Outcome, u64, u64) {
        let idle = Duration::from_secs(self.config.session_idle_timeout_secs.max(1));
        let drain = Duration::from_secs(self.config.finish_drain_secs.max(1));
        let mut heartbeat = tokio::time::interval(Duration::from_secs(self.config.heartbeat_secs.max(1)));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat.tick().await; // the first tick is immediate

        let mut last_activity = Instant::now();
        let mut audio_bytes: u64 = 0;
        let mut segments: u64 = 0;
        let mut finishing: Option<Instant> = None;
        let mut client_open = true;

        loop {
            let idle_at = last_activity + idle;
            let drain_at = finishing.map(|f| f + drain);
            tokio::select! {
                biased;

                event = stream.events.recv() => {
                    match event {
                        Some(TranscriptEvent::Segment(segment)) => {
                            segments += 1;
                            last_activity = Instant::now();
                            send(socket, &ServerMessage::Segment { session_id: session_id.to_string(), segment }).await;
                        }
                        Some(TranscriptEvent::Error { code, message, retryable }) => {
                            tracing::warn!(session_id, code = code.as_str(), message = message.as_str(), "transcription_stream: provider error");
                            send(socket, &ServerMessage::Error { code, message, retryable }).await;
                            return (Outcome::Error, audio_bytes, segments);
                        }
                        Some(TranscriptEvent::Ended) | None => {
                            send(socket, &ServerMessage::SessionEnded { session_id: session_id.to_string(), audio_ms: audio.duration_ms(audio_bytes), segments }).await;
                            return (Outcome::Ended, audio_bytes, segments);
                        }
                    }
                }

                message = socket.recv(), if client_open => {
                    match message {
                        Some(Ok(Message::Binary(frame))) => {
                            if finishing.is_some() {
                                continue;
                            }
                            audio_bytes += frame.len() as u64;
                            last_activity = Instant::now();
                            if let Err(e) = stream.audio.send_audio(Bytes::from(frame)).await {
                                send(socket, &ServerMessage::error("provider_error", format!("audio could not be delivered to the provider: {e:#}"), true)).await;
                                return (Outcome::Error, audio_bytes, segments);
                            }
                        }
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<ClientMessage>(&text) {
                                Ok(ClientMessage::AudioAppend { audio: encoded }) => {
                                    if finishing.is_some() {
                                        continue;
                                    }
                                    let frame = match base64::engine::general_purpose::STANDARD.decode(encoded.as_bytes()) {
                                        Ok(frame) => frame,
                                        Err(e) => {
                                            send(socket, &ServerMessage::error("protocol_error", format!("audio.append is not valid base64: {e}"), false)).await;
                                            return (Outcome::Error, audio_bytes, segments);
                                        }
                                    };
                                    audio_bytes += frame.len() as u64;
                                    last_activity = Instant::now();
                                    if let Err(e) = stream.audio.send_audio(Bytes::from(frame)).await {
                                        send(socket, &ServerMessage::error("provider_error", format!("audio could not be delivered to the provider: {e:#}"), true)).await;
                                        return (Outcome::Error, audio_bytes, segments);
                                    }
                                }
                                Ok(ClientMessage::SessionEnd) => {
                                    if finishing.is_none() {
                                        finishing = Some(Instant::now());
                                        if let Err(e) = stream.audio.finish().await {
                                            tracing::warn!(session_id, error = format!("{e:#}"), "transcription_stream: finish failed; ending with what was delivered");
                                            send(socket, &ServerMessage::SessionEnded { session_id: session_id.to_string(), audio_ms: audio.duration_ms(audio_bytes), segments }).await;
                                            return (Outcome::Ended, audio_bytes, segments);
                                        }
                                    }
                                }
                                Ok(ClientMessage::SessionStart { .. }) => {
                                    send(socket, &ServerMessage::error("protocol_error", "session.start was already received", false)).await;
                                    return (Outcome::Error, audio_bytes, segments);
                                }
                                Err(e) => {
                                    send(socket, &ServerMessage::error("protocol_error", format!("unrecognised message: {e}"), false)).await;
                                    return (Outcome::Error, audio_bytes, segments);
                                }
                            }
                        }
                        Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_))) | None => {
                            // The caller went away. Finish the provider side so
                            // the usage row is right, but nobody is reading, so
                            // no more segments are sent.
                            client_open = false;
                            if finishing.is_none() {
                                finishing = Some(Instant::now());
                                let _ = stream.audio.finish().await;
                            }
                        }
                        Some(Err(e)) => {
                            tracing::info!(session_id, error = %e, "transcription_stream: caller socket error");
                            client_open = false;
                            if finishing.is_none() {
                                finishing = Some(Instant::now());
                                let _ = stream.audio.finish().await;
                            }
                        }
                    }
                }

                _ = heartbeat.tick() => {
                    if client_open && socket.send(Message::Ping(Vec::new())).await.is_err() {
                        client_open = false;
                    }
                }

                _ = tokio::time::sleep_until(idle_at) => {
                    tracing::warn!(session_id, idle_secs = idle.as_secs(), "transcription_stream: session idle past the configured bound");
                    send(socket, &ServerMessage::error("idle_timeout", format!("no audio and no transcript for {}s", idle.as_secs()), false)).await;
                    return (Outcome::Error, audio_bytes, segments);
                }

                _ = async { tokio::time::sleep_until(drain_at.expect("guarded")).await }, if drain_at.is_some() => {
                    tracing::warn!(session_id, drain_secs = drain.as_secs(), "transcription_stream: provider did not finish within the drain window; ending with what was delivered");
                    send(socket, &ServerMessage::SessionEnded { session_id: session_id.to_string(), audio_ms: audio.duration_ms(audio_bytes), segments }).await;
                    return (Outcome::Ended, audio_bytes, segments);
                }
            }
        }
    }

    /// One usage row and one ledger row per session, metered in audio
    /// seconds. Fire-and-forget like the other audio routes.
    fn record_usage(&self, request_model: String, routed_model: String, audio_ms: u64) {
        use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

        let seconds = (audio_ms as f64 / 1000.0).round() as i64;
        let (cost, estimated) = match self.config.usd_per_audio_minute {
            Some(rate) => (rate * audio_ms as f64 / 60_000.0, false),
            None => (0.0, true),
        };
        let db = self.state.db.clone();
        let prompt_db = self.state.prompt_db.clone();
        let storage = self.state.storage.clone();
        let user_id = self.user.id;
        let api_key_id = self.user.api_key_id;
        let project = self.attribution.project_or(self.user.api_key_project.clone());
        let correlation = self.attribution.correlation_id.clone();
        let tags = self.attribution.tags_json();
        let provider = self.provider.clone();
        tokio::spawn(async move {
            let prompt = NewPrompt {
                user_id,
                session_id: None,
                request_model: request_model.clone(),
                routed_model: routed_model.clone(),
                provider: provider.clone(),
                messages: "[]".to_string(),
                response: None,
                finish_reason: None,
                prompt_tokens: seconds,
                completion_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: cost,
                latency_ms: None,
                ttft_ms: None,
                attempts: None,
                tags: "[]".to_string(),
                project: project.clone(),
                attribution_correlation_id: correlation.clone(),
                attribution_tags: tags.clone(),
                experiment_id: None,
                experiment_variant: None,
                call_kind: CallKind::Audio,
            };
            let stored = match crate::db::prompt_store::apply_storage_policy(&storage.load(), prompt) {
                Some(p) => match PromptRepository::create(&*prompt_db, p).await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::error!("Failed to log transcription session: {e}");
                        None
                    }
                },
                None => None,
            };
            let ledger = NewCostLedgerEntry {
                user_id,
                prompt_id: stored.as_ref().map(|s| s.id),
                model: routed_model,
                provider,
                project,
                tokens_in: seconds,
                tokens_out: 0,
                cost_usd: cost,
                api_key_id,
                attribution_correlation_id: correlation,
                attribution_tags: tags,
                experiment_id: None,
                experiment_variant: None,
                call_kind: CallKind::Audio,
                tokens_estimated: estimated,
                // A live session has no request body to fingerprint: the audio
                // arrives as frames over its lifetime.
                request_fingerprint: None,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            };
            if let Err(e) = CostRepository::create(&*db, ledger).await {
                tracing::error!("Failed to record transcription session cost: {e}");
            }
        });
    }
}

struct StartParams {
    session_id: String,
    model: Option<String>,
    audio: AudioFormat,
    language: Option<String>,
}

fn validate_audio(audio: &AudioFormat) -> Result<(), ServerMessage> {
    if !(8_000..=48_000).contains(&audio.sample_rate_hz) {
        return Err(ServerMessage::error(
            "protocol_error",
            format!("audio.sample_rate_hz must be between 8000 and 48000, got {}", audio.sample_rate_hz),
            false,
        ));
    }
    if !(1..=2).contains(&audio.channels) {
        return Err(ServerMessage::error(
            "protocol_error",
            format!("audio.channels must be 1 or 2, got {}", audio.channels),
            false,
        ));
    }
    Ok(())
}

async fn send(socket: &mut WebSocket, message: &ServerMessage) {
    let text = serde_json::to_string(message).expect("server messages serialise");
    if let Err(e) = socket.send(Message::Text(text)).await {
        tracing::debug!(error = %e, "transcription_stream: caller socket write failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::transcription::AudioEncoding;

    #[test]
    fn client_messages_parse_by_type() {
        let start: ClientMessage = serde_json::from_str(
            r#"{"type":"session.start","session_id":"s1","audio":{"encoding":"pcm16","sample_rate_hz":16000,"channels":1},"language":"en"}"#,
        )
        .unwrap();
        match start {
            ClientMessage::SessionStart { session_id, model, audio, language } => {
                assert_eq!(session_id, "s1");
                assert_eq!(model, None);
                assert_eq!(audio, AudioFormat { encoding: AudioEncoding::Pcm16, sample_rate_hz: 16_000, channels: 1 });
                assert_eq!(language.as_deref(), Some("en"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(serde_json::from_str::<ClientMessage>(r#"{"type":"session.end"}"#).unwrap(), ClientMessage::SessionEnd));
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"session.start","session_id":"s","audio":{"encoding":"opus","sample_rate_hz":1,"channels":1}}"#).is_err());
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"nope"}"#).is_err());
    }

    #[test]
    fn server_messages_flatten_the_segment() {
        let text = serde_json::to_string(&ServerMessage::Segment {
            session_id: "s1".into(),
            segment: TranscriptSegment {
                segment_id: "i1".into(),
                text: "hi".into(),
                is_final: true,
                start_ms: Some(1),
                end_ms: None,
                confidence: Some(0.9),
                speaker: None,
            },
        })
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["type"], "transcript.segment");
        assert_eq!(value["segment_id"], "i1");
        assert_eq!(value["text"], "hi");
        assert_eq!(value["is_final"], true);
        assert_eq!(value["start_ms"], 1);
        assert!(value["end_ms"].is_null());
        assert!(value["speaker"].is_null());
    }

    #[test]
    fn audio_validation_bounds_rate_and_channels() {
        let ok = AudioFormat { encoding: AudioEncoding::Pcm16, sample_rate_hz: 16_000, channels: 2 };
        assert!(validate_audio(&ok).is_ok());
        let low = AudioFormat { sample_rate_hz: 4_000, ..ok };
        assert!(matches!(validate_audio(&low).unwrap_err(), ServerMessage::Error { code, .. } if code == "protocol_error"));
        let many = AudioFormat { channels: 3, ..ok };
        assert!(validate_audio(&many).is_err());
    }
}
