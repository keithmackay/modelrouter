//! Streaming first-byte fallback.
//!
//! Some upstreams accept a streaming request and then send nothing: no
//! response headers, or headers and no body, while the connection stays
//! alive (TCP keepalives answered). The same request sent non-streamed
//! answers normally. An adapter that opens its upstream stream through
//! [`open_within`] learns within a bounded window whether the stream has
//! started; when it has not, the adapter abandons it and answers with its
//! non-streamed call, re-emitted by [`completion_sse`] as the SSE stream a
//! live one would have sent, so the caller sees an ordinary stream.
//!
//! Only the opening is bounded. Once the first chunk arrives the stream runs
//! under the request's own timeout, however long it takes, and nothing has
//! reached the caller before the fallback decision, so answering again
//! cannot duplicate output.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};

use crate::providers::adapter::{CompletionResult, SseStream};

/// An upstream response body as raw chunks.
pub type ByteStream = Pin<Box<dyn Stream<Item = anyhow::Result<Bytes>> + Send>>;

/// How an upstream stream opened.
pub enum Opened {
    /// The first chunk arrived in time; the stream replays it, then the rest.
    Started(ByteStream),
    /// Nothing arrived within the window.
    Silent,
}

/// Open an upstream stream with `open` and wait for its first chunk. A zero
/// `window` waits without limit. An error from `open` (a refused request, a
/// non-success status) is returned as is, never treated as silence.
pub async fn open_within<F>(window: Duration, open: F) -> anyhow::Result<Opened>
where
    F: Future<Output = anyhow::Result<ByteStream>>,
{
    let first_chunk = async {
        let mut stream = open.await?;
        let first = stream.next().await;
        anyhow::Ok((first, stream))
    };
    let (first, rest) = if window.is_zero() {
        first_chunk.await?
    } else {
        match tokio::time::timeout(window, first_chunk).await {
            Ok(opened) => opened?,
            Err(_) => return Ok(Opened::Silent),
        }
    };
    Ok(Opened::Started(Box::pin(futures::stream::iter(first).chain(rest))))
}

/// A non-streamed completion as OpenAI-shaped SSE: the answer, a finish chunk
/// carrying the completion's usage, then `[DONE]`.
pub fn completion_sse(result: &CompletionResult, model: &str) -> SseStream {
    let created = chrono::Utc::now().timestamp();
    let mut chunks =
        crate::router::cache::stream::chat_replay_chunks(result, "chatcmpl-stream-fallback", model, created);
    let mut usage = serde_json::json!({
        "prompt_tokens": result.prompt_tokens,
        "completion_tokens": result.completion_tokens,
        "total_tokens": result.prompt_tokens + result.completion_tokens,
        "prompt_tokens_details": {
            "cached_tokens": result.cache_read_tokens,
            "cache_creation_tokens": result.cache_write_tokens
        }
    });
    if let Some(reasoning) = result.reasoning_tokens {
        usage["completion_tokens_details"] = serde_json::json!({ "reasoning_tokens": reasoning });
    }
    if let Some(finish) = chunks.last_mut() {
        finish["usage"] = usage;
    }
    let mut sse: String = chunks.iter().map(|chunk| format!("data: {chunk}\n\n")).collect();
    sse.push_str("data: [DONE]\n\n");
    Box::pin(futures::stream::once(async move { Ok(Bytes::from(sse)) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;

    fn chunks(items: Vec<&'static str>) -> ByteStream {
        Box::pin(futures::stream::iter(items.into_iter().map(|s| Ok(Bytes::from(s)))))
    }

    async fn body(stream: ByteStream) -> String {
        let parts: Vec<Bytes> = stream.try_collect().await.unwrap();
        parts.iter().map(|b| String::from_utf8_lossy(b).into_owned()).collect()
    }

    #[tokio::test]
    async fn a_stream_that_starts_in_time_is_replayed_whole() {
        let opened = open_within(Duration::from_secs(5), async { Ok(chunks(vec!["a", "b", "c"])) }).await.unwrap();
        let Opened::Started(stream) = opened else { panic!("expected the stream to start") };
        assert_eq!(body(stream).await, "abc");
    }

    #[tokio::test]
    async fn a_stream_that_sends_nothing_within_the_window_is_silent() {
        let never: ByteStream = Box::pin(futures::stream::pending());
        let opened = open_within(Duration::from_millis(50), async { Ok(never) }).await.unwrap();
        assert!(matches!(opened, Opened::Silent));
    }

    #[tokio::test]
    async fn an_upstream_that_never_answers_the_request_is_silent() {
        let opened = open_within(Duration::from_millis(50), futures::future::pending::<anyhow::Result<ByteStream>>())
            .await
            .unwrap();
        assert!(matches!(opened, Opened::Silent));
    }

    #[tokio::test]
    async fn a_slow_first_chunk_inside_the_window_starts_the_stream() {
        let slow: ByteStream = Box::pin(futures::stream::once(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(Bytes::from_static(b"late but in time"))
        }));
        let Opened::Started(stream) = open_within(Duration::from_secs(5), async { Ok(slow) }).await.unwrap() else {
            panic!("a first chunk inside the window must start the stream");
        };
        assert_eq!(body(stream).await, "late but in time");
    }

    #[tokio::test]
    async fn a_zero_window_waits_without_limit() {
        let slow: ByteStream = Box::pin(futures::stream::once(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(Bytes::from_static(b"x"))
        }));
        assert!(matches!(open_within(Duration::ZERO, async { Ok(slow) }).await.unwrap(), Opened::Started(_)));
    }

    #[tokio::test]
    async fn an_open_error_is_an_error_not_silence() {
        let err = open_within(Duration::from_secs(5), async { anyhow::bail!("upstream returned 503") })
            .await
            .err()
            .unwrap();
        assert_eq!(err.to_string(), "upstream returned 503");
    }

    #[tokio::test]
    async fn a_completion_replays_as_sse_with_usage_and_done() {
        let result = CompletionResult {
            content: "hello".into(),
            prompt_tokens: 120,
            completion_tokens: 7,
            finish_reason: "stop".into(),
            cache_read_tokens: 100,
            cache_write_tokens: 0,
            reasoning_tokens: None,
            ttft_ms: Some(900),
            tool_calls: None,
        };
        let text = body(completion_sse(&result, "m")).await;
        let events: Vec<&str> = text.split("\n\n").filter(|e| !e.is_empty()).collect();
        assert_eq!(events.len(), 3, "{text}");
        let first: serde_json::Value = serde_json::from_str(events[0].trim_start_matches("data: ")).unwrap();
        assert_eq!(first["choices"][0]["delta"]["content"], "hello");
        let finish: serde_json::Value = serde_json::from_str(events[1].trim_start_matches("data: ")).unwrap();
        assert_eq!(finish["choices"][0]["finish_reason"], "stop");
        assert_eq!(
            finish["usage"],
            serde_json::json!({
                "prompt_tokens": 120, "completion_tokens": 7, "total_tokens": 127,
                "prompt_tokens_details": {"cached_tokens": 100, "cache_creation_tokens": 0}
            })
        );
        assert_eq!(events[2], "data: [DONE]");
    }
}
