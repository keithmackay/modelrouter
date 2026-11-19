//! Streaming transcription against an Azure AI Foundry deployment over its
//! realtime WebSocket surface.
//!
//! ## Wire shape
//!
//! ```text
//! GET wss://{host}/openai/v1/realtime?intent=transcription
//! Authorization: Bearer <Entra token>   (or: api-key: <key>)
//! ```
//!
//! then, client to provider, JSON text frames: one session configuration
//! message naming the transcription model, `input_audio_buffer.append` with
//! base64 16-bit PCM at 24 kHz mono (the only input format the surface
//! documents), and `input_audio_buffer.commit` when the caller is done.
//! Provider to client: `input_audio_buffer.speech_started` /
//! `speech_stopped` (the utterance boundaries, in ms from the start of the
//! audio), `conversation.item.input_audio_transcription.delta` (text as it is
//! recognised), `...completed` (the utterance's final text, with per-token
//! log-probabilities when asked for) and `error`.
//!
//! The session configuration message exists in two documented shapes:
//! `session.update` with `session.type = "transcription"` (the GA surface) and
//! the older `transcription_session.update`. The events are the same under
//! both, so `[transcription] session_shape` picks the message and nothing else
//! changes.
//!
//! ## No network in this environment
//!
//! Written against the published surface; the development host has no route
//! to Azure. Everything below is covered by tests against a mock realtime
//! server (`tests/test_foundry_transcription.rs`) and instrumented where a
//! live deployment would first diverge: the resolved URL and credential mode,
//! the handshake status and body, and every upstream message that is not
//! recognised.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::config::schema::{ProviderConfig, TranscriptionConfig};
use crate::providers::azure_entra::TokenProvider;
use crate::providers::foundry::auth::FoundryAuth;
use crate::providers::foundry::endpoint::{FoundryEndpoint, FoundrySurface};
use crate::providers::transcription::{
    confidence_from_logprobs, AudioEncoding, AudioSink, StreamingTranscriptionAdapter, TranscriptEvent,
    TranscriptSegment, TranscriptionSessionParams, TranscriptionStream,
};

/// The one input sample rate the realtime surface documents for PCM.
pub const PROVIDER_SAMPLE_RATE_HZ: u32 = 24_000;

/// Path and query appended to the OpenAI-compatible surface for a
/// transcription-only realtime session.
const REALTIME_PATH: &str = "/realtime?intent=transcription";

/// Events bounded by the event channel: a provider that outruns the caller
/// by this many segments is a caller that stopped reading.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Which session configuration message to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionShape {
    Ga,
    Beta,
}

impl SessionShape {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ga" => Ok(Self::Ga),
            "beta" => Ok(Self::Beta),
            other => anyhow::bail!("[transcription] session_shape must be \"ga\" or \"beta\", got {other:?}"),
        }
    }
}

pub struct FoundryTranscriptionAdapter {
    url: String,
    auth: FoundryAuth,
    shape: SessionShape,
    connect_timeout: Duration,
}

impl FoundryTranscriptionAdapter {
    pub fn new(config: &ProviderConfig, transcription: &TranscriptionConfig) -> anyhow::Result<Self> {
        let endpoint = FoundryEndpoint::from_config(config)?;
        let timeout = Duration::from_secs(config.timeout_secs.max(1));
        let auth = FoundryAuth::entra_by_default("foundry", config, endpoint.scope(), timeout)?;
        Self::build(endpoint, auth, config, transcription)
    }

    /// Test hook: a caller-supplied token source, bypassing the Entra round
    /// trip, like the other Foundry adapters' `with_token_provider`.
    pub fn with_token_provider(
        config: &ProviderConfig,
        transcription: &TranscriptionConfig,
        token_provider: Arc<dyn TokenProvider>,
    ) -> anyhow::Result<Self> {
        let endpoint = FoundryEndpoint::from_config(config)?;
        Self::build(endpoint, FoundryAuth::Entra(token_provider), config, transcription)
    }

    fn build(
        endpoint: FoundryEndpoint,
        auth: FoundryAuth,
        config: &ProviderConfig,
        transcription: &TranscriptionConfig,
    ) -> anyhow::Result<Self> {
        let url = match transcription.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            Some(url) => url.to_string(),
            None => realtime_url(&endpoint)?,
        };
        let shape = SessionShape::parse(&transcription.session_shape)?;
        tracing::info!(
            url = url.as_str(),
            auth = auth.label(),
            session_shape = ?shape,
            "foundry: streaming transcription adapter ready"
        );
        Ok(Self { url, auth, shape, connect_timeout: Duration::from_secs(config.timeout_secs.max(1)) })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

/// The realtime WebSocket URL for a resolved endpoint: the OpenAI-compatible
/// surface with the scheme switched to WebSocket and the realtime path
/// appended. The Model Inference surface has no realtime route, so it is
/// refused by name rather than guessed at.
pub fn realtime_url(endpoint: &FoundryEndpoint) -> anyhow::Result<String> {
    if endpoint.surface() != FoundrySurface::OpenAIV1 {
        anyhow::bail!(
            "streaming transcription needs the OpenAI-compatible surface ({{endpoint}}/openai/v1); \
             the configured endpoint {} is the model-inference surface, which has no realtime route. \
             Set [transcription] url to the realtime WebSocket URL, or point foundry_endpoint at the resource host.",
            endpoint.base()
        );
    }
    let base = endpoint.base();
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        anyhow::bail!("foundry endpoint {base:?} is neither http:// nor https://");
    };
    let mut url = format!("{ws_base}{REALTIME_PATH}");
    if let Some(version) = endpoint.api_version() {
        url.push_str("&api-version=");
        url.push_str(version);
    }
    Ok(url)
}

/// The session configuration message for `shape`.
pub fn session_config_message(shape: SessionShape, model: &str, language: Option<&str>) -> Value {
    let mut transcription = json!({ "model": model });
    if let Some(language) = language.map(str::trim).filter(|l| !l.is_empty()) {
        transcription["language"] = json!(language);
    }
    match shape {
        SessionShape::Ga => json!({
            "type": "session.update",
            "session": {
                "type": "transcription",
                "audio": {
                    "input": {
                        "format": { "type": "audio/pcm", "rate": PROVIDER_SAMPLE_RATE_HZ },
                        "transcription": transcription,
                        "turn_detection": { "type": "server_vad" }
                    }
                },
                "include": ["item.input_audio_transcription.logprobs"]
            }
        }),
        SessionShape::Beta => json!({
            "type": "transcription_session.update",
            "session": {
                "input_audio_format": "pcm16",
                "input_audio_transcription": transcription,
                "turn_detection": { "type": "server_vad" },
                "include": ["item.input_audio_transcription.logprobs"]
            }
        }),
    }
}

/// The caller's PCM, converted to what the provider takes: mono at
/// [`PROVIDER_SAMPLE_RATE_HZ`] by averaging channels and linear interpolation.
/// Stateful so a frame boundary never drops a sample: an odd trailing byte
/// and the fractional read position carry over to the next frame.
pub struct PcmResampler {
    in_rate: u32,
    channels: u16,
    /// A dangling byte from the previous frame (frames need not align to
    /// samples).
    carry: Option<u8>,
    /// The last input sample of the previous frame, for interpolation across
    /// the boundary.
    last: Option<i16>,
    /// Fractional position into the input, in input samples, from `last`.
    position: f64,
}

impl PcmResampler {
    pub fn new(in_rate: u32, channels: u16) -> Self {
        Self { in_rate, channels: channels.max(1), carry: None, last: None, position: 0.0 }
    }

    /// Convert one frame of interleaved pcm16 to mono pcm16 at the provider's
    /// rate. At the provider's rate and mono the bytes pass through (apart
    /// from an odd-byte carry).
    pub fn convert(&mut self, frame: &[u8]) -> Vec<u8> {
        let mono = self.mono_samples(frame);
        if self.in_rate == PROVIDER_SAMPLE_RATE_HZ {
            return mono.iter().flat_map(|s| s.to_le_bytes()).collect();
        }
        let step = f64::from(self.in_rate) / f64::from(PROVIDER_SAMPLE_RATE_HZ);
        let mut out = Vec::with_capacity(mono.len() * 2 * 2);
        // The interpolation window is [last, mono...]; position 0 is `last`.
        let mut window: Vec<i16> = Vec::with_capacity(mono.len() + 1);
        match self.last {
            Some(last) => window.push(last),
            None => {
                if mono.is_empty() {
                    return out;
                }
                window.push(mono[0]);
                self.position = self.position.max(1.0);
            }
        }
        window.extend_from_slice(&mono);
        while self.position + 1.0 < window.len() as f64 {
            let index = self.position.floor() as usize;
            let frac = self.position - index as f64;
            let a = f64::from(window[index]);
            let b = f64::from(window[index + 1]);
            let sample = (a + (b - a) * frac).round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
            out.extend_from_slice(&sample.to_le_bytes());
            self.position += step;
        }
        // Keep the last input sample as the next window's origin.
        let consumed = window.len() - 1;
        self.position -= consumed as f64;
        self.last = window.last().copied();
        out
    }

    fn mono_samples(&mut self, frame: &[u8]) -> Vec<i16> {
        let mut bytes: Vec<u8> = Vec::with_capacity(frame.len() + 1);
        if let Some(carry) = self.carry.take() {
            bytes.push(carry);
        }
        bytes.extend_from_slice(frame);
        if bytes.len() % 2 == 1 {
            self.carry = bytes.pop();
        }
        let samples: Vec<i16> = bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        if self.channels == 1 {
            return samples;
        }
        let channels = usize::from(self.channels);
        samples
            .chunks(channels)
            .map(|frame| {
                let sum: i64 = frame.iter().map(|s| i64::from(*s)).sum();
                (sum / frame.len() as i64) as i16
            })
            .collect()
    }
}

/// What the reader task remembers per utterance.
#[derive(Default, Debug)]
struct ItemState {
    text: String,
    start_ms: Option<u64>,
    end_ms: Option<u64>,
    completed: bool,
}

/// The reader's view of the session: utterances by item id, in arrival order.
#[derive(Default)]
pub struct UpstreamState {
    items: Vec<(String, ItemState)>,
    /// Commit acknowledgements seen (`input_audio_buffer.committed`, or the
    /// empty-buffer refusal, which is the ack for a commit with nothing left).
    /// The sink's `finish` sends a commit; its ack, with no utterance pending,
    /// is what says the provider has delivered everything.
    pub commit_acks: usize,
}

impl UpstreamState {
    fn item(&mut self, id: &str) -> &mut ItemState {
        if let Some(index) = self.items.iter().position(|(item_id, _)| item_id == id) {
            return &mut self.items[index].1;
        }
        self.items.push((id.to_string(), ItemState::default()));
        &mut self.items.last_mut().expect("just pushed").1
    }

    /// Utterances the provider started and has not finished.
    pub fn pending(&self) -> usize {
        self.items.iter().filter(|(_, item)| !item.completed).count()
    }

    /// Map one upstream message to the events it produces. Unknown message
    /// types produce nothing and are logged at debug, so a surface change
    /// shows in the log before it shows as a missing transcript.
    pub fn map_message(&mut self, message: &Value) -> Vec<TranscriptEvent> {
        let kind = message["type"].as_str().unwrap_or("");
        let item_id = message["item_id"].as_str().unwrap_or("").to_string();
        match kind {
            "input_audio_buffer.speech_started" => {
                let item = self.item(&item_id);
                item.start_ms = message["audio_start_ms"].as_u64();
                vec![]
            }
            "input_audio_buffer.speech_stopped" => {
                let item = self.item(&item_id);
                item.end_ms = message["audio_end_ms"].as_u64();
                vec![]
            }
            "conversation.item.input_audio_transcription.delta" => {
                let item = self.item(&item_id);
                item.text.push_str(message["delta"].as_str().unwrap_or(""));
                vec![TranscriptEvent::Segment(TranscriptSegment {
                    segment_id: item_id,
                    text: item.text.clone(),
                    is_final: false,
                    start_ms: item.start_ms,
                    end_ms: item.end_ms,
                    confidence: None,
                    speaker: None,
                })]
            }
            "conversation.item.input_audio_transcription.completed" => {
                let logprobs: Vec<f64> = message["logprobs"]
                    .as_array()
                    .map(|entries| entries.iter().filter_map(|e| e["logprob"].as_f64()).collect())
                    .unwrap_or_default();
                let confidence = confidence_from_logprobs(&logprobs);
                let item = self.item(&item_id);
                item.completed = true;
                item.text = message["transcript"].as_str().unwrap_or(&item.text).to_string();
                vec![TranscriptEvent::Segment(TranscriptSegment {
                    segment_id: item_id,
                    text: item.text.clone(),
                    is_final: true,
                    start_ms: item.start_ms,
                    end_ms: item.end_ms,
                    confidence,
                    speaker: None,
                })]
            }
            "conversation.item.input_audio_transcription.failed" => {
                // One utterance the provider could not transcribe: the session
                // goes on. The caller gets what was recognised so far at zero
                // confidence, so it can ask a person to check that span.
                tracing::warn!(
                    item_id = item_id.as_str(),
                    error = %message["error"],
                    "foundry: transcription of one utterance failed"
                );
                let item = self.item(&item_id);
                item.completed = true;
                vec![TranscriptEvent::Segment(TranscriptSegment {
                    segment_id: item_id,
                    text: item.text.clone(),
                    is_final: true,
                    start_ms: item.start_ms,
                    end_ms: item.end_ms,
                    confidence: Some(0.0),
                    speaker: None,
                })]
            }
            "error" => {
                let error = &message["error"];
                let code = error["code"]
                    .as_str()
                    .or_else(|| error["type"].as_str())
                    .unwrap_or("provider_error")
                    .to_string();
                // Committing an empty buffer after the caller's end is not a
                // fault: there was nothing left to transcribe. It is the
                // commit's acknowledgement.
                if code.starts_with("input_audio_buffer_commit_empty") {
                    self.commit_acks += 1;
                    return vec![];
                }
                vec![TranscriptEvent::Error {
                    message: error["message"].as_str().unwrap_or("provider error").to_string(),
                    code,
                    retryable: false,
                }]
            }
            "input_audio_buffer.committed" => {
                // A committed buffer becomes an utterance the provider will
                // transcribe: pending until its `completed` (or `failed`).
                if !item_id.is_empty() {
                    self.item(&item_id);
                }
                self.commit_acks += 1;
                vec![]
            }
            "session.created" | "session.updated" | "transcription_session.created" | "transcription_session.updated"
            | "input_audio_buffer.cleared" | "conversation.item.created" | "conversation.item.added"
            | "conversation.item.done" => vec![],
            other => {
                tracing::debug!(message_type = other, "foundry: unrecognised realtime message ignored");
                vec![]
            }
        }
    }
}

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

struct FoundrySink {
    writer: WsSink,
    resampler: PcmResampler,
    /// Set before the final commit is sent, so the reader can tell that
    /// commit's acknowledgement from an automatic one.
    finished: Arc<AtomicBool>,
}

#[async_trait]
impl AudioSink for FoundrySink {
    async fn send_audio(&mut self, frame: Bytes) -> anyhow::Result<()> {
        let converted = self.resampler.convert(&frame);
        if converted.is_empty() {
            return Ok(());
        }
        let message = json!({
            "type": "input_audio_buffer.append",
            "audio": base64::engine::general_purpose::STANDARD.encode(converted),
        });
        self.writer
            .send(Message::Text(message.to_string()))
            .await
            .context("foundry: sending audio to the realtime socket")
    }

    async fn finish(&mut self) -> anyhow::Result<()> {
        self.finished.store(true, Ordering::SeqCst);
        self.writer
            .send(Message::Text(json!({ "type": "input_audio_buffer.commit" }).to_string()))
            .await
            .context("foundry: committing the audio buffer")
    }
}

#[async_trait]
impl StreamingTranscriptionAdapter for FoundryTranscriptionAdapter {
    async fn open(&self, params: &TranscriptionSessionParams) -> anyhow::Result<TranscriptionStream> {
        match params.audio.encoding {
            AudioEncoding::Pcm16 => {}
        }
        let mut request = self
            .url
            .as_str()
            .into_client_request()
            .with_context(|| format!("foundry: realtime URL {} is not a valid WebSocket URL", self.url))?;
        let (name, value) = self.auth.header().await.context("foundry: credential for the realtime socket")?;
        request.headers_mut().insert(
            name,
            value.parse().context("foundry: credential is not a valid header value")?,
        );
        let connected = tokio::time::timeout(self.connect_timeout, tokio_tungstenite::connect_async(request)).await;
        let (socket, response) = match connected {
            Ok(Ok(ok)) => ok,
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => {
                let status = response.status();
                let body = response
                    .body()
                    .as_ref()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                tracing::warn!(
                    url = self.url.as_str(),
                    status = status.as_u16(),
                    body = body.as_str(),
                    "foundry: realtime handshake refused"
                );
                anyhow::bail!("foundry realtime handshake refused with {status}: {body}");
            }
            Ok(Err(e)) => return Err(anyhow::Error::new(e).context("foundry: connecting to the realtime socket")),
            Err(_) => anyhow::bail!(
                "foundry: realtime handshake did not complete within {}s",
                self.connect_timeout.as_secs()
            ),
        };
        tracing::info!(
            session_id = params.session_id.as_str(),
            model = params.model.as_str(),
            handshake_status = response.status().as_u16(),
            in_sample_rate_hz = params.audio.sample_rate_hz,
            in_channels = params.audio.channels,
            "foundry: realtime transcription session open"
        );
        let (mut writer, mut reader) = socket.split();
        let config = session_config_message(self.shape, &params.model, params.language.as_deref());
        writer
            .send(Message::Text(config.to_string()))
            .await
            .context("foundry: sending the session configuration")?;

        let finished = Arc::new(AtomicBool::new(false));
        let (events, receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let reader_finished = finished.clone();
        let session_id = params.session_id.clone();
        tokio::spawn(async move {
            let mut state = UpstreamState::default();
            // The session is drained once the caller has finished, the
            // provider has acknowledged a commit sent after that, and no
            // utterance is pending. An ack before `finished` is an automatic
            // (VAD) commit, which only ever adds a pending utterance.
            let mut ack_after_finish = false;
            loop {
                let Some(message) = reader.next().await else {
                    // The provider closed first: whatever it delivered is all
                    // there is.
                    let _ = events.send(TranscriptEvent::Ended).await;
                    return;
                };
                let text = match message {
                    Ok(Message::Text(text)) => text,
                    Ok(Message::Close(frame)) => {
                        tracing::info!(session_id = session_id.as_str(), close = ?frame, "foundry: provider closed the realtime socket");
                        let _ = events.send(TranscriptEvent::Ended).await;
                        return;
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        let _ = events
                            .send(TranscriptEvent::Error {
                                code: "provider_socket_error".to_string(),
                                message: format!("foundry realtime socket failed: {e}"),
                                retryable: true,
                            })
                            .await;
                        return;
                    }
                };
                let value: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(session_id = session_id.as_str(), error = %e, "foundry: non-JSON realtime message ignored");
                        continue;
                    }
                };
                let was_finished = reader_finished.load(Ordering::SeqCst);
                let acks_before = state.commit_acks;
                for event in state.map_message(&value) {
                    let fatal = matches!(event, TranscriptEvent::Error { .. });
                    if events.send(event).await.is_err() || fatal {
                        return;
                    }
                }
                if was_finished && state.commit_acks > acks_before {
                    ack_after_finish = true;
                }
                if ack_after_finish && state.pending() == 0 {
                    let _ = events.send(TranscriptEvent::Ended).await;
                    return;
                }
            }
        });

        Ok(TranscriptionStream {
            audio: Box::new(FoundrySink {
                writer,
                resampler: PcmResampler::new(params.audio.sample_rate_hz, params.audio.channels),
                finished,
            }),
            events: receiver,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(url: &str) -> FoundryEndpoint {
        FoundryEndpoint::from_config(&ProviderConfig { foundry_endpoint: Some(url.to_string()), ..Default::default() })
            .unwrap()
    }

    #[test]
    fn realtime_url_switches_scheme_and_appends_the_route() {
        assert_eq!(
            realtime_url(&endpoint("https://r.services.ai.azure.com")).unwrap(),
            "wss://r.services.ai.azure.com/openai/v1/realtime?intent=transcription"
        );
        assert_eq!(
            realtime_url(&endpoint("http://127.0.0.1:9/openai/v1/")).unwrap(),
            "ws://127.0.0.1:9/openai/v1/realtime?intent=transcription"
        );
    }

    #[test]
    fn realtime_url_carries_a_configured_api_version() {
        let ep = FoundryEndpoint::from_config(&ProviderConfig {
            foundry_endpoint: Some("https://r.openai.azure.com".to_string()),
            api_version: Some("2025-04-01-preview".to_string()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            realtime_url(&ep).unwrap(),
            "wss://r.openai.azure.com/openai/v1/realtime?intent=transcription&api-version=2025-04-01-preview"
        );
    }

    #[test]
    fn model_inference_surface_is_refused_by_name() {
        let err = realtime_url(&endpoint("https://r.services.ai.azure.com/models")).unwrap_err();
        assert!(err.to_string().contains("model-inference surface"), "{err}");
    }

    #[test]
    fn session_shapes_name_the_model_and_language() {
        let ga = session_config_message(SessionShape::Ga, "transcribe-1", Some("en"));
        assert_eq!(ga["type"], "session.update");
        assert_eq!(ga["session"]["type"], "transcription");
        assert_eq!(ga["session"]["audio"]["input"]["transcription"]["model"], "transcribe-1");
        assert_eq!(ga["session"]["audio"]["input"]["transcription"]["language"], "en");
        assert_eq!(ga["session"]["audio"]["input"]["format"]["rate"], PROVIDER_SAMPLE_RATE_HZ);
        let beta = session_config_message(SessionShape::Beta, "transcribe-1", None);
        assert_eq!(beta["type"], "transcription_session.update");
        assert_eq!(beta["session"]["input_audio_format"], "pcm16");
        assert_eq!(beta["session"]["input_audio_transcription"]["model"], "transcribe-1");
        assert!(beta["session"]["input_audio_transcription"].get("language").is_none());
        assert_eq!(SessionShape::parse(" GA ").unwrap(), SessionShape::Ga);
        assert!(SessionShape::parse("v2").is_err());
    }

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn samples(bytes: &[u8]) -> Vec<i16> {
        bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
    }

    #[test]
    fn resampler_passes_provider_rate_mono_through() {
        let mut r = PcmResampler::new(PROVIDER_SAMPLE_RATE_HZ, 1);
        let input = pcm(&[1, -2, 3, 30000]);
        assert_eq!(r.convert(&input), input);
    }

    #[test]
    fn resampler_carries_an_odd_byte_across_frames() {
        let mut r = PcmResampler::new(PROVIDER_SAMPLE_RATE_HZ, 1);
        let input = pcm(&[100, 200, 300]);
        let first = r.convert(&input[..3]);
        let second = r.convert(&input[3..]);
        let mut joined = first;
        joined.extend(second);
        assert_eq!(joined, input);
    }

    #[test]
    fn resampler_downmixes_stereo_by_averaging() {
        let mut r = PcmResampler::new(PROVIDER_SAMPLE_RATE_HZ, 2);
        let out = r.convert(&pcm(&[100, 300, -50, 50]));
        assert_eq!(samples(&out), vec![200, 0]);
    }

    #[test]
    fn resampler_upsamples_16k_to_24k_at_three_to_two() {
        let mut r = PcmResampler::new(16_000, 1);
        // One second of a ramp: 16,000 input samples should become ~24,000.
        let input: Vec<i16> = (0..16_000).map(|i| (i % 1000) as i16).collect();
        let mut total = 0;
        for chunk in pcm(&input).chunks(640) {
            total += samples(&r.convert(chunk)).len();
        }
        assert!((23_990..=24_000).contains(&total), "{total}");
    }

    #[test]
    fn resampler_interpolates_between_samples() {
        // 12 kHz -> 24 kHz doubles: every other output sample is a midpoint.
        let mut r = PcmResampler::new(12_000, 1);
        let out = samples(&r.convert(&pcm(&[0, 100, 200, 300])));
        assert_eq!(&out[..6], &[0, 50, 100, 150, 200, 250]);
    }

    fn msg(json: Value) -> Value {
        json
    }

    #[test]
    fn upstream_messages_map_to_partial_then_final_segments() {
        let mut state = UpstreamState::default();
        assert!(state
            .map_message(&msg(json!({"type":"input_audio_buffer.speech_started","item_id":"i1","audio_start_ms":120})))
            .is_empty());
        assert_eq!(state.pending(), 1);
        let partial = state.map_message(&msg(json!({"type":"conversation.item.input_audio_transcription.delta","item_id":"i1","delta":"hello"})));
        let partial2 = state.map_message(&msg(json!({"type":"conversation.item.input_audio_transcription.delta","item_id":"i1","delta":" there"})));
        assert_eq!(
            partial,
            vec![TranscriptEvent::Segment(TranscriptSegment {
                segment_id: "i1".into(),
                text: "hello".into(),
                is_final: false,
                start_ms: Some(120),
                end_ms: None,
                confidence: None,
                speaker: None,
            })]
        );
        match &partial2[0] {
            TranscriptEvent::Segment(s) => assert_eq!(s.text, "hello there"),
            other => panic!("{other:?}"),
        }
        state.map_message(&msg(json!({"type":"input_audio_buffer.speech_stopped","item_id":"i1","audio_end_ms":980})));
        let half = (0.5f64).ln();
        let done = state.map_message(&msg(json!({
            "type":"conversation.item.input_audio_transcription.completed","item_id":"i1",
            "transcript":"Hello there.","logprobs":[{"token":"Hello","logprob":half},{"token":" there.","logprob":half}]
        })));
        match &done[0] {
            TranscriptEvent::Segment(s) => {
                assert!(s.is_final);
                assert_eq!(s.text, "Hello there.");
                assert_eq!((s.start_ms, s.end_ms), (Some(120), Some(980)));
                assert!((s.confidence.unwrap() - 0.5).abs() < 1e-9);
                assert_eq!(s.speaker, None);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(state.pending(), 0);
    }

    #[test]
    fn a_failed_utterance_is_a_final_zero_confidence_segment_not_a_session_error() {
        let mut state = UpstreamState::default();
        state.map_message(&msg(json!({"type":"input_audio_buffer.speech_started","item_id":"i2","audio_start_ms":5})));
        state.map_message(&msg(json!({"type":"conversation.item.input_audio_transcription.delta","item_id":"i2","delta":"par"})));
        let out = state.map_message(&msg(json!({"type":"conversation.item.input_audio_transcription.failed","item_id":"i2","error":{"message":"x"}})));
        match &out[0] {
            TranscriptEvent::Segment(s) => {
                assert!(s.is_final);
                assert_eq!(s.text, "par");
                assert_eq!(s.confidence, Some(0.0));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(state.pending(), 0);
    }

    #[test]
    fn session_errors_map_to_error_events_except_the_empty_commit() {
        let mut state = UpstreamState::default();
        let out = state.map_message(&msg(json!({"type":"error","error":{"type":"invalid_request_error","code":"unknown_parameter","message":"bad"}})));
        assert_eq!(
            out,
            vec![TranscriptEvent::Error { code: "unknown_parameter".into(), message: "bad".into(), retryable: false }]
        );
        let out = state.map_message(&msg(json!({"type":"error","error":{"type":"invalid_request_error","message":"no code"}})));
        assert!(matches!(&out[0], TranscriptEvent::Error { code, .. } if code == "invalid_request_error"));
        assert!(state
            .map_message(&msg(json!({"type":"error","error":{"code":"input_audio_buffer_commit_empty","message":"buffer too small"}})))
            .is_empty());
    }

    #[test]
    fn lifecycle_and_unknown_messages_produce_nothing() {
        let mut state = UpstreamState::default();
        for kind in ["session.created", "session.updated", "something.new"] {
            assert!(state.map_message(&msg(json!({"type": kind}))).is_empty(), "{kind}");
        }
        assert_eq!(state.pending(), 0);
        assert_eq!(state.commit_acks, 0);
    }

    #[test]
    fn a_commit_ack_counts_and_its_item_is_pending_until_completed() {
        let mut state = UpstreamState::default();
        assert!(state.map_message(&msg(json!({"type":"input_audio_buffer.committed","item_id":"i9"}))).is_empty());
        assert_eq!((state.commit_acks, state.pending()), (1, 1));
        state.map_message(&msg(json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"i9","transcript":"ok"})));
        assert_eq!(state.pending(), 0);
        // The empty-buffer refusal is the ack for a commit with nothing left.
        assert!(state
            .map_message(&msg(json!({"type":"error","error":{"code":"input_audio_buffer_commit_empty","message":"buffer too small"}})))
            .is_empty());
        assert_eq!(state.commit_acks, 2);
    }
}
