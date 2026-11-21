//! Streaming transcription: the provider-neutral seam behind
//! `GET /v1/audio/transcriptions/stream`.
//!
//! A session is one long-lived conversation: audio frames flow in for as long
//! as the caller keeps talking, transcript segments flow out as the provider
//! recognises them. That shape is nothing like a request/response adapter, so
//! it gets its own pair of halves rather than a method on `ProviderAdapter`:
//!
//! - an [`AudioSink`] the route writes audio into and finishes;
//! - a channel of [`TranscriptEvent`]s the route forwards to the caller.
//!
//! Splitting the two lets the route `select!` over client frames, provider
//! events, the heartbeat and the idle deadline without a shared `&mut` across
//! the await points. A provider implements [`StreamingTranscriptionAdapter`]
//! and owns whatever socket, process or batch loop produces the events.

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// How the caller's audio is encoded. Only little-endian 16-bit PCM today;
/// an enum so a provider can refuse an encoding it does not take and a future
/// Opus path has somewhere to live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioEncoding {
    Pcm16,
}

/// The audio format a session declares in `session.start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFormat {
    pub encoding: AudioEncoding,
    pub sample_rate_hz: u32,
    pub channels: u16,
}

impl AudioFormat {
    /// Bytes per second of audio in this format.
    pub fn bytes_per_second(&self) -> u64 {
        match self.encoding {
            AudioEncoding::Pcm16 => u64::from(self.sample_rate_hz) * u64::from(self.channels) * 2,
        }
    }

    /// Milliseconds of audio `bytes` bytes carry in this format.
    pub fn duration_ms(&self, bytes: u64) -> u64 {
        let per_second = self.bytes_per_second();
        if per_second == 0 {
            return 0;
        }
        bytes * 1000 / per_second
    }
}

/// What a provider needs to open a session.
#[derive(Debug, Clone)]
pub struct TranscriptionSessionParams {
    /// The caller's session id: an identifier for logs, never content.
    pub session_id: String,
    /// The deployment or model name the provider serves it with.
    pub model: String,
    pub audio: AudioFormat,
    /// BCP-47 language hint, when the caller gave one.
    pub language: Option<String>,
}

/// One recognised span of speech. Partial segments (`is_final: false`) carry
/// the text so far for the utterance identified by `segment_id`; the final one
/// for that id replaces them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub segment_id: String,
    pub text: String,
    pub is_final: bool,
    /// Offset from the start of the session's audio, when the provider says.
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    /// 0..1 when the provider reports one (derived from log-probabilities
    /// where that is all it gives).
    pub confidence: Option<f64>,
    /// The provider's speaker label, when it diarises.
    pub speaker: Option<String>,
}

/// What flows from a provider session to the route.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEvent {
    Segment(TranscriptSegment),
    /// The provider has delivered everything for the audio it was given.
    Ended,
    /// The provider failed; `retryable` says whether a fresh session might
    /// succeed (a transient upstream fault) or not (a rejected configuration).
    Error { code: String, message: String, retryable: bool },
}

/// The half of a session the route writes into.
#[async_trait]
pub trait AudioSink: Send {
    /// Deliver one frame of audio in the session's declared format.
    async fn send_audio(&mut self, frame: Bytes) -> anyhow::Result<()>;
    /// No more audio is coming: flush, and have the provider finish the
    /// segments for what it already has. Events keep flowing until `Ended`.
    async fn finish(&mut self) -> anyhow::Result<()>;
}

/// An open provider session: the sink and the event channel.
pub struct TranscriptionStream {
    pub audio: Box<dyn AudioSink>,
    pub events: mpsc::Receiver<TranscriptEvent>,
}

/// A provider that can serve streaming transcription.
#[async_trait]
pub trait StreamingTranscriptionAdapter: Send + Sync {
    /// Opens a session against the provider. An error here is reported to the
    /// caller as `provider_error` before any audio is accepted.
    async fn open(&self, params: &TranscriptionSessionParams) -> anyhow::Result<TranscriptionStream>;
}

/// Confidence from per-token log-probabilities: the geometric mean of the
/// token probabilities, i.e. `exp(mean(logprob))`, clamped to 0..1. `None`
/// when there are no tokens, so an empty utterance does not read as certain.
pub fn confidence_from_logprobs(logprobs: &[f64]) -> Option<f64> {
    if logprobs.is_empty() {
        return None;
    }
    let mean = logprobs.iter().sum::<f64>() / logprobs.len() as f64;
    Some(mean.exp().clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm16_duration_counts_samples_not_bytes() {
        let f = AudioFormat { encoding: AudioEncoding::Pcm16, sample_rate_hz: 16_000, channels: 1 };
        // 16 kHz mono pcm16 = 32,000 bytes per second.
        assert_eq!(f.bytes_per_second(), 32_000);
        assert_eq!(f.duration_ms(32_000), 1_000);
        assert_eq!(f.duration_ms(16_000), 500);
        let stereo = AudioFormat { encoding: AudioEncoding::Pcm16, sample_rate_hz: 16_000, channels: 2 };
        assert_eq!(stereo.duration_ms(32_000), 500);
    }

    #[test]
    fn zero_rate_never_divides_by_zero() {
        let f = AudioFormat { encoding: AudioEncoding::Pcm16, sample_rate_hz: 0, channels: 1 };
        assert_eq!(f.duration_ms(1_000), 0);
    }

    #[test]
    fn confidence_is_the_geometric_mean_probability() {
        assert_eq!(confidence_from_logprobs(&[]), None);
        // Every token certain: confidence 1.
        assert_eq!(confidence_from_logprobs(&[0.0, 0.0]), Some(1.0));
        // ln(0.5) for every token: confidence 0.5.
        let half = (0.5f64).ln();
        let c = confidence_from_logprobs(&[half, half, half]).unwrap();
        assert!((c - 0.5).abs() < 1e-9, "{c}");
        // A positive logprob (a provider rounding artefact) cannot exceed 1.
        assert_eq!(confidence_from_logprobs(&[0.1]), Some(1.0));
    }
}
