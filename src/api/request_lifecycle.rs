//! Per-request upstream lifecycle logging: one INFO line when a provider call
//! starts, one when its response headers arrive (streamed calls), and one when
//! it finishes, however it finishes.
//!
//! Without these lines, a call the upstream accepted and never answered was
//! invisible: nothing was logged until the call failed or completed, so an
//! operator could not tell "waiting on the provider" from "lost between the
//! caller and the router". Every line carries the router's request id and the
//! caller's attribution correlation id (when sent), so the router's log joins
//! the caller's.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use futures::Stream;

use crate::providers::adapter::SseStream;

/// One upstream provider call. Dropping it before [`finish`](Self::finish)
/// logs the call as `dropped`, e.g. when the caller disconnects mid-stream.
pub struct UpstreamCall {
    request_id: String,
    correlation_id: Option<String>,
    provider: String,
    model: String,
    streaming: bool,
    started: Instant,
    headers_ms: Option<u128>,
    finished: bool,
}

impl UpstreamCall {
    pub fn start(
        request_id: &str,
        correlation_id: Option<&str>,
        provider: &str,
        model: &str,
        streaming: bool,
    ) -> Self {
        tracing::info!(
            request_id,
            correlation_id = correlation_id.unwrap_or(""),
            provider,
            model,
            streaming,
            "upstream request started"
        );
        Self {
            request_id: request_id.to_string(),
            correlation_id: correlation_id.map(str::to_string),
            provider: provider.to_string(),
            model: model.to_string(),
            streaming,
            started: Instant::now(),
            headers_ms: None,
            finished: false,
        }
    }

    pub fn elapsed_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    /// The upstream answered with a success status and its body is about to flow.
    pub fn headers(&mut self) {
        let headers_ms = self.elapsed_ms();
        self.headers_ms = Some(headers_ms);
        tracing::info!(
            request_id = self.request_id.as_str(),
            correlation_id = self.correlation_id.as_deref().unwrap_or(""),
            provider = self.provider.as_str(),
            model = self.model.as_str(),
            headers_ms = headers_ms as u64,
            "upstream response headers received"
        );
    }

    /// `outcome` is `completed`, `failed`, `stream_error` or `dropped`.
    pub fn finish(&mut self, outcome: &str, error: Option<&str>) {
        if self.finished {
            return;
        }
        self.finished = true;
        tracing::info!(
            request_id = self.request_id.as_str(),
            correlation_id = self.correlation_id.as_deref().unwrap_or(""),
            provider = self.provider.as_str(),
            model = self.model.as_str(),
            streaming = self.streaming,
            outcome,
            latency_ms = self.elapsed_ms() as u64,
            headers_ms = self.headers_ms.map(|ms| ms as i64).unwrap_or(-1),
            error = error.unwrap_or(""),
            "upstream request finished"
        );
    }
}

impl Drop for UpstreamCall {
    fn drop(&mut self) {
        self.finish("dropped", None);
    }
}

/// Wraps a streamed body so the call logs `completed` at the end of the
/// stream, `stream_error` on the first error item, and `dropped` if the body
/// is abandoned first.
pub struct LifecycleStream {
    inner: SseStream,
    call: UpstreamCall,
}

impl LifecycleStream {
    pub fn new(inner: SseStream, call: UpstreamCall) -> Self {
        Self { inner, call }
    }
}

impl Stream for LifecycleStream {
    type Item = anyhow::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let polled = self.inner.as_mut().poll_next(cx);
        match &polled {
            Poll::Ready(None) => self.call.finish("completed", None),
            Poll::Ready(Some(Err(e))) => {
                let message = e.to_string();
                self.call.finish("stream_error", Some(&message));
            }
            _ => {}
        }
        polled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Runs `body` with a JSON log subscriber and returns the events it logged.
    fn logged<F: FnOnce()>(body: F) -> Vec<serde_json::Value> {
        let out = Captured::default();
        let subscriber = tracing_subscriber::registry()
            .with(crate::logging::fmt_layer(crate::config::schema::LogFormat::Json, out.clone()));
        tracing::subscriber::with_default(subscriber, body);
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn messages(events: &[serde_json::Value]) -> Vec<String> {
        events.iter().map(|e| e["message"].as_str().unwrap().to_string()).collect()
    }

    fn body_of(chunks: Vec<anyhow::Result<Bytes>>) -> SseStream {
        Box::pin(futures::stream::iter(chunks))
    }

    #[test]
    fn a_streamed_call_logs_start_headers_and_completion() {
        let events = logged(|| {
            let mut call = UpstreamCall::start("req-1", Some("corr-1"), "vertex", "claude", true);
            call.headers();
            let stream = LifecycleStream::new(body_of(vec![Ok(Bytes::from("data: x\n\n"))]), call);
            futures::executor::block_on(stream.collect::<Vec<_>>());
        });
        assert_eq!(
            messages(&events),
            ["upstream request started", "upstream response headers received", "upstream request finished"]
        );
        assert!(events.iter().all(|e| e["request_id"] == "req-1" && e["correlation_id"] == "corr-1"));
        assert_eq!(events[2]["outcome"], "completed");
        assert!(events[2]["headers_ms"].as_i64().unwrap() >= 0);
    }

    #[test]
    fn a_call_still_waiting_on_the_upstream_shows_a_start_and_nothing_else() {
        let events = logged(|| {
            let call = UpstreamCall::start("req-2", None, "vertex", "claude", true);
            std::mem::forget(call);
        });
        assert_eq!(messages(&events), ["upstream request started"]);
    }

    #[test]
    fn a_stream_error_is_logged_once_with_its_message() {
        let events = logged(|| {
            let mut call = UpstreamCall::start("req-3", None, "vertex", "claude", true);
            call.headers();
            let stream = LifecycleStream::new(
                body_of(vec![Err(anyhow::anyhow!("connection reset")), Ok(Bytes::from("late"))]),
                call,
            );
            futures::executor::block_on(stream.collect::<Vec<_>>());
        });
        let finished: Vec<_> = events.iter().filter(|e| e["message"] == "upstream request finished").collect();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0]["outcome"], "stream_error");
        assert_eq!(finished[0]["error"], "connection reset");
    }

    #[test]
    fn an_abandoned_stream_is_logged_as_dropped() {
        let events = logged(|| {
            let call = UpstreamCall::start("req-4", None, "vertex", "claude", true);
            drop(LifecycleStream::new(body_of(vec![Ok(Bytes::from("data: x\n\n"))]), call));
        });
        assert_eq!(events.last().unwrap()["outcome"], "dropped");
        assert_eq!(events.last().unwrap()["headers_ms"], -1);
    }

    #[test]
    fn a_failed_call_logs_failed_with_the_error() {
        let events = logged(|| {
            let mut call = UpstreamCall::start("req-5", None, "vertex", "claude", false);
            call.finish("failed", Some("Vertex AI returned 400"));
        });
        assert_eq!(events.len(), 2);
        assert_eq!(events[1]["outcome"], "failed");
        assert_eq!(events[1]["error"], "Vertex AI returned 400");
        assert_eq!(events[1]["streaming"], false);
    }
}
