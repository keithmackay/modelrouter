//! Token usage read from an Anthropic Messages event stream as it passes
//! through untouched.
//!
//! Anthropic splits a stream's usage: `message_start` carries the input and
//! prompt-cache counts, and `message_delta` carries the final output count.
//! The counts are complete only once `message_stop` arrives; a stream cut
//! short leaves the output count at whatever was last reported.

use serde_json::Value;

use crate::providers::sse_lines::SseLineBuffer;

#[derive(Debug, Default)]
pub struct MessagesStreamUsage {
    lines: SseLineBuffer,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
    saw_usage: bool,
    done: bool,
}

impl MessagesStreamUsage {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        for line in self.lines.push(chunk) {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(data.trim()) else {
                continue;
            };
            match event["type"].as_str() {
                Some("message_start") => self.absorb(&event["message"]["usage"]),
                Some("message_delta") => self.absorb(&event["usage"]),
                Some("message_stop") => self.done = true,
                _ => {}
            }
        }
    }

    fn absorb(&mut self, usage: &Value) {
        if !usage.is_object() {
            return;
        }
        self.saw_usage = true;
        let count = |field: &str| usage[field].as_u64().map(|n| n as u32);
        if let Some(n) = count("input_tokens") {
            self.input_tokens = n;
        }
        if let Some(n) = count("output_tokens") {
            self.output_tokens = n;
        }
        if let Some(n) = count("cache_read_input_tokens") {
            self.cache_read_tokens = n;
        }
        if let Some(n) = count("cache_creation_input_tokens") {
            self.cache_write_tokens = n;
        }
    }

    /// True once any usage has been reported.
    pub fn saw_usage(&self) -> bool {
        self.saw_usage
    }

    /// True when the stream reached `message_stop` with its usage reported,
    /// so the counts are the provider's final ones.
    pub fn is_complete(&self) -> bool {
        self.saw_usage && self.done
    }
}

#[cfg(test)]
mod tests {
    use super::MessagesStreamUsage;
    use serde_json::json;

    fn stream(events: &[serde_json::Value]) -> String {
        events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect()
    }

    fn full() -> String {
        stream(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 20,
                "cache_read_input_tokens": 500, "cache_creation_input_tokens": 30, "output_tokens": 1}}}),
            json!({"type": "ping"}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 9}}),
            json!({"type": "message_stop"}),
        ])
    }

    #[test]
    fn reads_input_cache_and_final_output_counts() {
        let mut usage = MessagesStreamUsage::new();
        // Byte-at-a-time: the harshest chunking there is.
        for b in full().as_bytes() {
            usage.feed(std::slice::from_ref(b));
        }
        assert!(usage.is_complete());
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.cache_read_tokens, usage.cache_write_tokens),
            (20, 9, 500, 30)
        );
    }

    #[test]
    fn a_stream_cut_short_keeps_what_arrived_and_is_not_complete() {
        let cut = full().replace("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n", "");
        let mut usage = MessagesStreamUsage::new();
        usage.feed(cut.as_bytes());
        assert!(usage.saw_usage());
        assert!(!usage.is_complete());
        assert_eq!((usage.input_tokens, usage.output_tokens), (20, 9));
    }

    #[test]
    fn a_stream_without_usage_is_never_complete() {
        let mut usage = MessagesStreamUsage::new();
        usage.feed(stream(&[json!({"type": "message_stop"})]).as_bytes());
        assert!(!usage.saw_usage());
        assert!(!usage.is_complete());
    }

    #[test]
    fn unparseable_lines_are_skipped() {
        let mut usage = MessagesStreamUsage::new();
        usage.feed(b"data: not json\n\n");
        usage.feed(full().as_bytes());
        assert!(usage.is_complete());
        assert_eq!(usage.output_tokens, 9);
    }
}
