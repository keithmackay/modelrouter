//! Capture and replay of streamed responses.
//!
//! A stream that completes cleanly is assembled into the same payload a
//! non-streamed call would have stored, so the two share one cache entry: a
//! streamed request can hit an entry recorded from a plain call and the other
//! way round. On a hit for a `stream: true` request the entry is replayed as
//! SSE in the wire format the request used.
//!
//! Capture is deliberately strict. Anything the assembler does not fully
//! understand (an unparseable `data:` line, a second choice, an error event, an
//! unknown delta type) marks the capture unusable, and an unusable capture is
//! never stored. A missed store costs one provider call; a wrong store replays
//! a wrong answer until it expires.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::providers::adapter::CompletionResult;
use crate::providers::sse_lines::SseLineBuffer;

// ── OpenAI chat/completions ───────────────────────────────────────────────────

#[derive(Debug, Default)]
struct ToolCallAcc {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Folds an OpenAI-shaped `chat.completion.chunk` stream (what every chat
/// adapter emits) into a [`CompletionResult`].
#[derive(Debug, Default)]
pub struct ChatStreamCapture {
    lines: SseLineBuffer,
    content: String,
    tool_calls: BTreeMap<u64, ToolCallAcc>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    done: bool,
    unusable: bool,
}

impl ChatStreamCapture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        for line in self.lines.push(chunk) {
            self.feed_line(&line);
        }
    }

    /// True once the terminal `[DONE]` has been read.
    pub fn is_done(&self) -> bool {
        self.done
    }

    fn feed_line(&mut self, line: &str) {
        if self.unusable || self.done {
            return;
        }
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data == "[DONE]" {
            self.done = true;
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            self.unusable = true;
            return;
        };
        if chunk.get("error").is_some_and(|e| !e.is_null()) {
            self.unusable = true;
            return;
        }
        if chunk["usage"].is_object() {
            self.usage = Some(chunk["usage"].clone());
        }
        for choice in chunk["choices"].as_array().into_iter().flatten() {
            if choice["index"].as_u64().unwrap_or(0) != 0 {
                // `n > 1`: the stored result holds one choice.
                self.unusable = true;
                return;
            }
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str() {
                self.content.push_str(text);
            }
            for call in delta["tool_calls"].as_array().into_iter().flatten() {
                let acc = self
                    .tool_calls
                    .entry(call["index"].as_u64().unwrap_or(0))
                    .or_default();
                if let Some(id) = call["id"].as_str() {
                    acc.id = Some(id.to_string());
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    acc.name.push_str(name);
                }
                if let Some(args) = call["function"]["arguments"].as_str() {
                    acc.arguments.push_str(args);
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.finish_reason = Some(reason.to_string());
            }
        }
    }

    /// The assembled result, or `None` when the stream is not safe to store:
    /// it never reached `[DONE]`, carried something unparseable, or reported
    /// no usage (a cached entry without real token counts would misstate
    /// usage and savings on every hit).
    pub fn finish(self) -> Option<CompletionResult> {
        if !self.done || self.unusable || self.lines.has_partial_line() {
            return None;
        }
        let usage = self.usage?;
        let prompt_tokens = usage["prompt_tokens"].as_u64()? as u32;
        let completion_tokens = usage["completion_tokens"].as_u64()? as u32;
        let tool_calls = if self.tool_calls.is_empty() {
            None
        } else {
            let mut calls = Vec::with_capacity(self.tool_calls.len());
            for acc in self.tool_calls.into_values() {
                calls.push(json!({
                    "id": acc.id?,
                    "type": "function",
                    "function": {"name": acc.name, "arguments": acc.arguments},
                }));
            }
            Some(Value::Array(calls))
        };
        Some(CompletionResult {
            content: self.content,
            prompt_tokens,
            completion_tokens,
            finish_reason: self.finish_reason.unwrap_or_else(|| "stop".to_string()),
            cache_read_tokens: usage["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0) as u32,
            cache_write_tokens: usage["prompt_tokens_details"]["cache_creation_tokens"]
                .as_u64()
                .unwrap_or(0) as u32,
            reasoning_tokens: usage["completion_tokens_details"]["reasoning_tokens"]
                .as_u64()
                .map(|n| n as u32),
            ttft_ms: None,
            tool_calls,
        })
    }
}

/// The `chat.completion.chunk` objects that replay `result`: the assistant
/// turn in one delta, then the finish chunk. The caller appends its usage
/// chunk and `data: [DONE]`.
pub fn chat_replay_chunks(
    result: &CompletionResult,
    id: &str,
    model: &str,
    created: i64,
) -> Vec<Value> {
    let chunk = |delta: Value, finish_reason: Value| {
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        })
    };
    let mut delta = json!({"role": "assistant", "content": result.content});
    if let Some(calls) = result.tool_calls.as_ref().and_then(Value::as_array) {
        let indexed: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, call)| {
                let mut call = call.clone();
                call["index"] = json!(i);
                call
            })
            .collect();
        delta["tool_calls"] = Value::Array(indexed);
        if result.content.is_empty() {
            delta["content"] = Value::Null;
        }
    }
    vec![
        chunk(delta, Value::Null),
        chunk(json!({}), json!(result.finish_reason)),
    ]
}

// ── Anthropic /v1/messages ────────────────────────────────────────────────────

#[derive(Debug)]
struct BlockAcc {
    block: Value,
    partial_json: String,
}

/// Folds an Anthropic Messages event stream into the `message` object a
/// non-streamed `/v1/messages` call returns.
#[derive(Debug, Default)]
pub struct MessagesStreamCapture {
    lines: SseLineBuffer,
    message: Option<Value>,
    blocks: BTreeMap<u64, BlockAcc>,
    done: bool,
    unusable: bool,
}

impl MessagesStreamCapture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        for line in self.lines.push(chunk) {
            self.feed_line(&line);
        }
    }

    /// True once `message_stop` has been read.
    pub fn is_done(&self) -> bool {
        self.done
    }

    fn feed_line(&mut self, line: &str) {
        if self.unusable || self.done {
            return;
        }
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let Ok(event) = serde_json::from_str::<Value>(data.trim()) else {
            self.unusable = true;
            return;
        };
        if self.apply(&event).is_none() {
            self.unusable = true;
        }
    }

    /// Apply one event. `None` means the event was not understood.
    fn apply(&mut self, event: &Value) -> Option<()> {
        match event["type"].as_str()? {
            "ping" => {}
            "message_start" => {
                self.message = Some(event["message"].as_object()?.clone().into());
            }
            "content_block_start" => {
                let index = event["index"].as_u64()?;
                self.blocks.insert(
                    index,
                    BlockAcc {
                        block: event["content_block"].as_object()?.clone().into(),
                        partial_json: String::new(),
                    },
                );
            }
            "content_block_delta" => {
                let acc = self.blocks.get_mut(&event["index"].as_u64()?)?;
                let delta = &event["delta"];
                match delta["type"].as_str()? {
                    "text_delta" => append_str(&mut acc.block, "text", delta["text"].as_str()?),
                    "thinking_delta" => {
                        append_str(&mut acc.block, "thinking", delta["thinking"].as_str()?)
                    }
                    "signature_delta" => {
                        acc.block["signature"] = json!(delta["signature"].as_str()?);
                    }
                    "input_json_delta" => {
                        acc.partial_json.push_str(delta["partial_json"].as_str()?)
                    }
                    "citations_delta" => {
                        let citations = acc
                            .block
                            .as_object_mut()?
                            .entry("citations")
                            .or_insert_with(|| json!([]));
                        if !citations.is_array() {
                            *citations = json!([]);
                        }
                        citations.as_array_mut()?.push(delta["citation"].clone());
                    }
                    _ => return None,
                }
            }
            "content_block_stop" => {}
            "message_delta" => {
                let message = self.message.as_mut()?;
                for (k, v) in event["delta"].as_object()? {
                    message[k.as_str()] = v.clone();
                }
                if let Some(usage) = event["usage"].as_object() {
                    let target = message
                        .as_object_mut()?
                        .entry("usage")
                        .or_insert_with(|| json!({}));
                    for (k, v) in usage {
                        if !v.is_null() {
                            target[k.as_str()] = v.clone();
                        }
                    }
                }
            }
            "message_stop" => self.done = true,
            _ => return None,
        }
        Some(())
    }

    /// The assembled `message`, or `None` when the stream is not safe to
    /// store (never reached `message_stop`, carried an error or an event this
    /// capture does not understand, or a tool input that is not valid JSON).
    pub fn finish(self) -> Option<Value> {
        if !self.done || self.unusable || self.lines.has_partial_line() {
            return None;
        }
        let mut message = self.message?;
        message["stop_reason"].as_str()?;
        message["usage"].as_object()?;
        let mut content = Vec::with_capacity(self.blocks.len());
        for acc in self.blocks.into_values() {
            let mut block = acc.block;
            if block["type"] == "tool_use" || !acc.partial_json.is_empty() {
                block["input"] = if acc.partial_json.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&acc.partial_json).ok()?
                };
            }
            content.push(block);
        }
        message["content"] = Value::Array(content);
        Some(message)
    }
}

fn append_str(block: &mut Value, field: &str, text: &str) {
    let mut current = block[field].as_str().unwrap_or_default().to_string();
    current.push_str(text);
    block[field] = Value::String(current);
}

/// Replay a stored Anthropic `message` as the Messages API event stream:
/// `message_start`, one start/delta/stop triple per content block,
/// `message_delta` with the stop reason and final usage, `message_stop`.
pub fn messages_replay_sse(message: &Value) -> String {
    let mut out = String::new();
    let mut emit = |event: Value| {
        let name = event["type"].as_str().unwrap_or_default().to_string();
        out.push_str(&format!("event: {name}\ndata: {event}\n\n"));
    };

    let usage = message["usage"].clone();
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["stop_sequence"] = Value::Null;
    if start["usage"].is_object() {
        start["usage"]["output_tokens"] = json!(0);
    }
    emit(json!({"type": "message_start", "message": start}));

    for (index, block) in message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let (opening, deltas) = replay_block(block);
        emit(json!({"type": "content_block_start", "index": index, "content_block": opening}));
        for delta in deltas {
            emit(json!({"type": "content_block_delta", "index": index, "delta": delta}));
        }
        emit(json!({"type": "content_block_stop", "index": index}));
    }

    emit(json!({
        "type": "message_delta",
        "delta": {
            "stop_reason": message["stop_reason"],
            "stop_sequence": message.get("stop_sequence").cloned().unwrap_or(Value::Null),
        },
        "usage": usage,
    }));
    emit(json!({"type": "message_stop"}));
    out
}

/// The `content_block_start` payload for `block` and the deltas that rebuild
/// it. Block types with no delta form (redacted thinking, server tool
/// results) open complete, as the live API sends them.
fn replay_block(block: &Value) -> (Value, Vec<Value>) {
    let without = |fields: &[&str], empty: Value| {
        let mut opening: Map<String, Value> = block.as_object().cloned().unwrap_or_default();
        for f in fields {
            opening.remove(*f);
        }
        let mut opening = Value::Object(opening);
        if let (Some(obj), Some(empty)) = (opening.as_object_mut(), empty.as_object()) {
            obj.extend(empty.clone());
        }
        opening
    };
    match block["type"].as_str().unwrap_or_default() {
        "text" => {
            let mut deltas = vec![json!({"type": "text_delta", "text": block["text"]})];
            for citation in block["citations"].as_array().into_iter().flatten() {
                deltas.push(json!({"type": "citations_delta", "citation": citation}));
            }
            (without(&["text", "citations"], json!({"text": ""})), deltas)
        }
        "thinking" => {
            let mut deltas = vec![json!({"type": "thinking_delta", "thinking": block["thinking"]})];
            if let Some(sig) = block["signature"].as_str() {
                deltas.push(json!({"type": "signature_delta", "signature": sig}));
            }
            (
                without(&["thinking", "signature"], json!({"thinking": ""})),
                deltas,
            )
        }
        "tool_use" | "server_tool_use" => {
            let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
            let deltas =
                vec![json!({"type": "input_json_delta", "partial_json": input.to_string()})];
            (without(&["input"], json!({"input": {}})), deltas)
        }
        _ => (block.clone(), Vec::new()),
    }
}

/// Token accounting of a stored Anthropic `message`, as the
/// [`CompletionResult`] the cache-hit ledger path records.
pub fn message_as_completion(message: &Value) -> CompletionResult {
    let usage = &message["usage"];
    let tokens = |field: &str| usage[field].as_u64().unwrap_or(0) as u32;
    let content = message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["text"].as_str())
        .collect::<String>();
    CompletionResult {
        content,
        prompt_tokens: tokens("input_tokens"),
        completion_tokens: tokens("output_tokens"),
        finish_reason: message["stop_reason"]
            .as_str()
            .unwrap_or("end_turn")
            .to_string(),
        cache_read_tokens: tokens("cache_read_input_tokens"),
        cache_write_tokens: tokens("cache_creation_input_tokens"),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(lines: &[Value]) -> String {
        lines.iter().map(|l| format!("data: {l}\n\n")).collect()
    }

    fn chat_stream() -> String {
        let mut s = sse(&[
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hel"}, "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {"content": "lo"}, "finish_reason": "stop"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 12, "completion_tokens": 2,
                "prompt_tokens_details": {"cached_tokens": 4},
                "completion_tokens_details": {"reasoning_tokens": 1}}}),
        ]);
        s.push_str("data: [DONE]\n\n");
        s
    }

    #[test]
    fn chat_capture_assembles_text_usage_and_finish_reason() {
        let mut cap = ChatStreamCapture::new();
        cap.feed(chat_stream().as_bytes());
        assert!(cap.is_done());
        let r = cap.finish().expect("clean stream is storable");
        assert_eq!(r.content, "Hello");
        assert_eq!(r.finish_reason, "stop");
        assert_eq!(
            (r.prompt_tokens, r.completion_tokens, r.cache_read_tokens),
            (12, 2, 4)
        );
        assert_eq!(r.reasoning_tokens, Some(1));
        assert!(r.tool_calls.is_none());
    }

    #[test]
    fn chat_capture_survives_arbitrary_chunk_boundaries() {
        let stream = chat_stream();
        for split in [1, 7, 33, stream.len() - 3] {
            let mut cap = ChatStreamCapture::new();
            cap.feed(&stream.as_bytes()[..split]);
            cap.feed(&stream.as_bytes()[split..]);
            assert_eq!(
                cap.finish().expect("storable").content,
                "Hello",
                "split at {split}"
            );
        }
    }

    #[test]
    fn chat_capture_assembles_tool_calls() {
        let mut s = sse(&[
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": ""}}]}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"city\":"}}]}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"Oslo\"}"}}]}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 5}}),
        ]);
        s.push_str("data: [DONE]\n\n");
        let mut cap = ChatStreamCapture::new();
        cap.feed(s.as_bytes());
        let r = cap.finish().unwrap();
        assert_eq!(r.finish_reason, "tool_calls");
        let calls = r.tool_calls.unwrap();
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(calls[0]["function"]["arguments"], "{\"city\":\"Oslo\"}");
    }

    #[test]
    fn chat_capture_refuses_incomplete_or_suspect_streams() {
        let unfinished = chat_stream().replace("data: [DONE]\n\n", "");
        let garbage = chat_stream().replacen("data: {", "data: {not json", 1);
        let errored = format!(
            "data: {}\n\n{}",
            json!({"error": {"message": "boom"}}),
            chat_stream()
        );
        let second_choice = format!(
            "data: {}\n\n{}",
            json!({"choices": [{"index": 1, "delta": {"content": "x"}}]}),
            chat_stream()
        );
        let no_usage = sse(&[
            json!({"choices": [{"index": 0, "delta": {"content": "x"}, "finish_reason": "stop"}]}),
        ]) + "data: [DONE]\n\n";
        for (name, stream) in [
            ("unfinished", unfinished),
            ("garbage", garbage),
            ("errored", errored),
            ("second choice", second_choice),
            ("no usage", no_usage),
        ] {
            let mut cap = ChatStreamCapture::new();
            cap.feed(stream.as_bytes());
            assert!(cap.finish().is_none(), "{name} must not be stored");
        }
    }

    #[test]
    fn chat_replay_round_trips_through_the_capture() {
        let result = CompletionResult {
            content: String::new(),
            prompt_tokens: 3,
            completion_tokens: 5,
            finish_reason: "tool_calls".to_string(),
            tool_calls: Some(json!([{"id": "call_1", "type": "function",
                "function": {"name": "f", "arguments": "{}"}}])),
            ..Default::default()
        };
        let chunks = chat_replay_chunks(&result, "chatcmpl-x", "m", 1);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert!(
            chunks[0]["choices"][0]["delta"]["content"].is_null(),
            "pure tool turn"
        );
        assert_eq!(chunks[1]["choices"][0]["finish_reason"], "tool_calls");

        let mut stream = sse(&chunks);
        stream.push_str(&sse(&[
            json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 5}}),
        ]));
        stream.push_str("data: [DONE]\n\n");
        let mut cap = ChatStreamCapture::new();
        cap.feed(stream.as_bytes());
        let back = cap.finish().unwrap();
        assert_eq!(back.tool_calls, result.tool_calls);
        assert_eq!(back.finish_reason, "tool_calls");
    }

    fn anthropic_stream() -> String {
        let events = [
            json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant",
                "model": "claude-x", "content": [], "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 20, "cache_read_input_tokens": 5, "output_tokens": 1}}}),
            json!({"type": "ping"}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Hi "}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "there"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "tu_1", "name": "f", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"a\":"}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "1}"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null}, "usage": {"output_tokens": 9}}),
            json!({"type": "message_stop"}),
        ];
        events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect()
    }

    #[test]
    fn messages_capture_assembles_the_message() {
        let stream = anthropic_stream();
        let mut cap = MessagesStreamCapture::new();
        // Byte-at-a-time: the harshest chunking there is.
        for b in stream.as_bytes() {
            cap.feed(std::slice::from_ref(b));
        }
        assert!(cap.is_done());
        let m = cap.finish().expect("storable");
        assert_eq!(m["id"], "msg_1");
        assert_eq!(m["stop_reason"], "tool_use");
        assert_eq!(m["usage"]["input_tokens"], 20);
        assert_eq!(m["usage"]["output_tokens"], 9);
        assert_eq!(
            m["content"][0],
            json!({"type": "thinking", "thinking": "hmm", "signature": "sig"})
        );
        assert_eq!(m["content"][1], json!({"type": "text", "text": "Hi there"}));
        assert_eq!(
            m["content"][2],
            json!({"type": "tool_use", "id": "tu_1", "name": "f", "input": {"a": 1}})
        );
    }

    #[test]
    fn messages_capture_refuses_incomplete_or_errored_streams() {
        let unfinished = anthropic_stream().replace(
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            "",
        );
        let errored = anthropic_stream().replace(
            "{\"type\":\"ping\"}",
            "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}",
        );
        let unknown_delta = anthropic_stream().replace("text_delta", "mystery_delta");
        let bad_tool_json = anthropic_stream().replace("\"1}\"", "\"1\"");
        for (name, stream) in [
            ("unfinished", unfinished),
            ("errored", errored),
            ("unknown delta", unknown_delta),
            ("bad tool json", bad_tool_json),
        ] {
            let mut cap = MessagesStreamCapture::new();
            cap.feed(stream.as_bytes());
            assert!(cap.finish().is_none(), "{name} must not be stored");
        }
    }

    #[test]
    fn messages_replay_round_trips_through_the_capture() {
        let mut cap = MessagesStreamCapture::new();
        cap.feed(anthropic_stream().as_bytes());
        let original = cap.finish().unwrap();

        let replay = messages_replay_sse(&original);
        assert!(replay.starts_with("event: message_start\n"));
        assert!(replay.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
        let mut again = MessagesStreamCapture::new();
        again.feed(replay.as_bytes());
        assert_eq!(
            again.finish().expect("replay is itself a valid stream"),
            original
        );
    }

    #[test]
    fn message_accounting_reads_anthropic_usage() {
        let m = json!({"content": [{"type": "text", "text": "a"}, {"type": "tool_use"}, {"type": "text", "text": "b"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 7, "output_tokens": 3, "cache_read_input_tokens": 2, "cache_creation_input_tokens": 1}});
        let r = message_as_completion(&m);
        assert_eq!(r.content, "ab");
        assert_eq!(
            (
                r.prompt_tokens,
                r.completion_tokens,
                r.cache_read_tokens,
                r.cache_write_tokens
            ),
            (7, 3, 2, 1)
        );
        assert_eq!(r.finish_reason, "end_turn");
    }
}
