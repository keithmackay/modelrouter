//! Tool-carrying chat for the deployments that refuse function tools on
//! `/chat/completions`, served over the OpenAI Responses API instead.
//!
//! The newest OpenAI reasoning generations answer a chat completion that
//! carries function tools with:
//!
//! ```text
//! 400 Bad Request
//! Function tools with reasoning_effort are not supported for <deployment> in
//! /v1/chat/completions. To use function tools, use /v1/responses or set
//! reasoning_effort to 'none'.
//! ```
//!
//! Turning reasoning off would silently downgrade the model a caller chose
//! for its reasoning, so the adapter takes the other door: a request that
//! carries tools, for a deployment in the family table
//! ([`crate::router::model_capabilities::tools_via_responses_api`], overridable
//! per model in `[[model_capabilities]]`), is posted to
//! `POST {endpoint}/openai/v1/responses` with the same credential, and its
//! answer is translated back to the router's result shape. A request without
//! tools stays on chat completions, where the same deployment answers.
//!
//! ## Wire shape, both ways
//!
//! Request: chat `messages` become `input` items (`system`/`developer`/`user`
//! messages as message items with `input_text`/`input_image` parts, an
//! assistant `tool_calls` turn as `function_call` items, a `tool`-role result
//! as `function_call_output`); chat tools (`{type, function: {name, …}}`)
//! flatten to Responses tools (`{type, name, …}`), `tool_choice` likewise;
//! `max_tokens` is `max_output_tokens`, `reasoning_effort` is
//! `reasoning.effort`, `response_format` is `text.format`. `store` is sent
//! `false`: the router never retrieves a stored response, so the service has
//! nothing to keep.
//!
//! Response: the `output` items' `output_text` parts are the content,
//! `function_call` items are the chat `tool_calls` (keyed by `call_id`, the
//! id the caller echoes back), `usage.input_tokens`/`output_tokens` with
//! `input_tokens_details.cached_tokens` and
//! `output_tokens_details.reasoning_tokens` are the usage, and an
//! `incomplete` status for `max_output_tokens` is `finish_reason: "length"`.
//!
//! Streaming: the Responses event stream is translated to OpenAI
//! `chat.completion.chunk` frames, the contract every chat adapter's stream
//! meets (see the Anthropic translator): a `function_call` item opening is a
//! tool-call header chunk, argument deltas are argument fragments against
//! that index, text deltas are content, and `response.completed` becomes the
//! final chunk with `usage` followed by `data: [DONE]`.

use anyhow::Context;
use bytes::Bytes;
use futures::TryStreamExt;
use serde_json::{json, Value};

use crate::providers::adapter::{CompletionResult, NormalizedRequest, SseStream};
use crate::providers::foundry::adapter::{openai_reasoning_effort, FoundryAdapter};

/// The Responses request for `req`, `stream` set as asked.
pub fn build_body(req: &NormalizedRequest, stream: bool) -> Value {
    let mut body = json!({
        "model": req.model,
        "input": input_items(&req.messages),
        "stream": stream,
        "store": false,
    });
    if let Some(temp) = req.temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(max) = req.max_tokens {
        body["max_output_tokens"] = json!(max);
    }
    if let Some(effort) = openai_reasoning_effort(req) {
        body["reasoning"] = json!({ "effort": effort });
    }
    if let Some(tools) = req.tools.as_ref().filter(|t| !t.is_empty()) {
        body["tools"] = Value::Array(tools.iter().map(responses_tool).collect());
        if let Some(tc) = &req.tool_choice {
            body["tool_choice"] = responses_tool_choice(tc);
        }
    }
    if let Some(rf) = &req.response_format {
        body["text"] = json!({ "format": text_format(rf) });
    }
    body
}

/// Chat messages as Responses `input` items.
fn input_items(messages: &[Value]) -> Vec<Value> {
    let mut items = Vec::with_capacity(messages.len());
    for message in messages {
        match message["role"].as_str().unwrap_or("user") {
            "tool" => items.push(json!({
                "type": "function_call_output",
                "call_id": message["tool_call_id"].clone(),
                "output": content_text(&message["content"]),
            })),
            "assistant" => {
                let text = content_text(&message["content"]);
                if !text.is_empty() {
                    items.push(json!({ "role": "assistant", "content": text }));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": call["id"].clone(),
                        "name": call["function"]["name"].clone(),
                        "arguments": arguments_string(&call["function"]["arguments"]),
                    }));
                }
            }
            role => items.push(json!({ "role": role, "content": input_content(&message["content"]) })),
        }
    }
    items
}

/// A message `content` for a non-assistant message: a string passes through;
/// chat parts become Responses input parts (`text` → `input_text`,
/// `image_url` → `input_image`); parts already in the Responses spelling, and
/// anything unknown, pass through.
fn input_content(content: &Value) -> Value {
    let Some(parts) = content.as_array() else {
        return match content {
            Value::Null => Value::String(String::new()),
            other => other.clone(),
        };
    };
    Value::Array(
        parts
            .iter()
            .map(|part| match part["type"].as_str() {
                Some("text") => json!({ "type": "input_text", "text": part["text"].clone() }),
                Some("image_url") => {
                    let url = match &part["image_url"] {
                        Value::String(s) => Value::String(s.clone()),
                        nested => nested["url"].clone(),
                    };
                    let mut out = json!({ "type": "input_image", "image_url": url });
                    if let Some(detail) = part["image_url"]["detail"].as_str() {
                        out["detail"] = json!(detail);
                    }
                    out
                }
                _ => part.clone(),
            })
            .collect(),
    )
}

/// The text of a message `content`: the string itself, or the text parts of
/// an array joined, or nothing.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str().or_else(|| p["refusal"].as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Function arguments as the JSON string both APIs carry. A caller that sent
/// an object (some SDKs do) is serialised; a missing value is an empty object.
fn arguments_string(arguments: &Value) -> String {
    match arguments {
        Value::String(s) => s.clone(),
        Value::Null => "{}".to_string(),
        other => other.to_string(),
    }
}

/// A chat tool (`{type: function, function: {name, description, parameters,
/// strict}}`) as a Responses tool (the same fields, flattened). The Responses
/// API defaults `strict` to true and chat completions to false; a caller that
/// said nothing gets the chat default it wrote its schema for, since a strict
/// schema needs `additionalProperties: false` and every property required.
/// A tool already in the flat shape passes through.
fn responses_tool(tool: &Value) -> Value {
    let Some(function) = tool.get("function").filter(|f| f.is_object()) else {
        return tool.clone();
    };
    let mut out = json!({ "type": "function", "name": function["name"].clone() });
    for key in ["description", "parameters"] {
        if let Some(v) = function.get(key).filter(|v| !v.is_null()) {
            out[key] = v.clone();
        }
    }
    out["strict"] = function.get("strict").cloned().unwrap_or(Value::Bool(false));
    out
}

/// `tool_choice` in the Responses spelling: a mode string passes through, a
/// named function (`{type: function, function: {name}}`) flattens.
fn responses_tool_choice(choice: &Value) -> Value {
    match choice["function"]["name"].as_str() {
        Some(name) if choice["type"] == "function" => json!({ "type": "function", "name": name }),
        _ => choice.clone(),
    }
}

/// `response_format` as `text.format`: `json_object` and `text` pass through,
/// `json_schema` flattens its `json_schema` object (name, schema, strict,
/// description) onto the format itself.
fn text_format(response_format: &Value) -> Value {
    if response_format["type"] != "json_schema" {
        return response_format.clone();
    }
    let schema = &response_format["json_schema"];
    let mut out = json!({ "type": "json_schema" });
    for key in ["name", "schema", "strict", "description"] {
        if let Some(v) = schema.get(key).filter(|v| !v.is_null()) {
            out[key] = v.clone();
        }
    }
    out
}

/// Usage in the OpenAI chat shape, from a Responses `usage` object, or `None`
/// when the payload reports no token counts.
fn chat_usage(usage: &Value) -> Option<Value> {
    if !usage.is_object() {
        return None;
    }
    let count = |v: &Value| v.as_u64().and_then(|n| u32::try_from(n).ok());
    let input = count(&usage["input_tokens"])?;
    let output = count(&usage["output_tokens"]).unwrap_or(0);
    let mut out = json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": input + output,
        "prompt_tokens_details": {
            "cached_tokens": count(&usage["input_tokens_details"]["cached_tokens"]).unwrap_or(0).min(input)
        }
    });
    if let Some(reasoning) = count(&usage["output_tokens_details"]["reasoning_tokens"]) {
        out["completion_tokens_details"] = json!({ "reasoning_tokens": reasoning });
    }
    Some(out)
}

/// The chat `finish_reason` for a finished response: `tool_calls` when the
/// model called a tool (OpenAI agentic clients dispatch on that literal),
/// `length` when the output was cut at `max_output_tokens`, `content_filter`
/// when the service filtered it, else `stop`.
fn finish_reason(response: &Value, called_tools: bool) -> &'static str {
    if called_tools {
        return "tool_calls";
    }
    if response["status"] == "incomplete" {
        return match response["incomplete_details"]["reason"].as_str() {
            Some("max_output_tokens") => "length",
            Some("content_filter") => "content_filter",
            _ => "stop",
        };
    }
    "stop"
}

/// A `function_call` output item as a chat `tool_calls` entry. The chat id is
/// the item's `call_id`: that is what the caller echoes in `tool_call_id`,
/// and what the next turn's `function_call_output` must name.
fn chat_tool_call(item: &Value) -> Value {
    let id = item["call_id"].as_str().or_else(|| item["id"].as_str()).unwrap_or_default();
    json!({
        "id": id,
        "type": "function",
        "function": {
            "name": item["name"].clone(),
            "arguments": arguments_string(&item["arguments"]),
        }
    })
}

/// The service's reason for a `failed` response, for the error message.
fn failure_message(response: &Value) -> String {
    match response["error"]["message"].as_str() {
        Some(m) => m.to_string(),
        None => response["error"].to_string(),
    }
}

/// Parse a non-streamed Responses payload into the router's result shape.
pub fn parse_response(v: Value) -> anyhow::Result<CompletionResult> {
    if !v.is_object() {
        anyhow::bail!("Azure AI Foundry returned a Responses body that is not an object");
    }
    if v["status"] == "failed" {
        anyhow::bail!("Azure AI Foundry reported the response as failed: {}", failure_message(&v));
    }
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    for item in v["output"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("message") => content.push_str(&content_text(&item["content"])),
            Some("function_call") => tool_calls.push(chat_tool_call(item)),
            _ => {}
        }
    }
    let usage = chat_usage(&v["usage"]).unwrap_or(Value::Null);
    let count = |v: &Value| v.as_u64().map(|n| n as u32);
    Ok(CompletionResult {
        content,
        prompt_tokens: count(&usage["prompt_tokens"]).unwrap_or(0),
        completion_tokens: count(&usage["completion_tokens"]).unwrap_or(0),
        finish_reason: finish_reason(&v, !tool_calls.is_empty()).to_string(),
        cache_read_tokens: count(&usage["prompt_tokens_details"]["cached_tokens"]).unwrap_or(0),
        cache_write_tokens: 0,
        reasoning_tokens: count(&usage["completion_tokens_details"]["reasoning_tokens"]),
        ttft_ms: None,
        tool_calls: (!tool_calls.is_empty()).then(|| Value::Array(tool_calls)),
    })
}

/// Translates one Responses event stream into OpenAI `chat.completion.chunk`
/// frames.
///
/// Stateful for two reasons: the Responses API numbers every output item
/// (reasoning and message items included) in `output_index`, while OpenAI
/// numbers only the tool calls, so each `function_call` item is given the next
/// chat index when it opens; and a server may deliver a call's arguments only
/// in the `done` event, so the translator remembers which calls streamed
/// deltas and emits the whole arguments for the ones that did not.
#[derive(Debug, Default)]
pub struct ResponsesSseTranslator {
    /// Responses `output_index` → OpenAI `tool_calls[].index`.
    tool_indices: std::collections::HashMap<u64, u64>,
    next_tool_index: u64,
    /// Chat indices that received at least one argument delta.
    streamed_arguments: std::collections::HashSet<u64>,
    /// True once the final chunk and `[DONE]` have been emitted; later lines
    /// carry nothing.
    finished: bool,
}

impl ResponsesSseTranslator {
    pub fn new() -> Self {
        Self::default()
    }

    fn chunk(choice: Value) -> Bytes {
        let chunk = json!({
            "id": "chatcmpl-stream",
            "object": "chat.completion.chunk",
            "choices": [choice]
        });
        Bytes::from(format!("data: {chunk}\n\n"))
    }

    /// A tool-call header chunk for a `function_call` item at `output_index`.
    fn open_call(&mut self, output_index: u64, item: &Value) -> Bytes {
        let index = self.next_tool_index;
        self.next_tool_index += 1;
        self.tool_indices.insert(output_index, index);
        let id = item["call_id"].as_str().or_else(|| item["id"].as_str()).unwrap_or_default();
        Self::chunk(json!({"index": 0, "delta": {"tool_calls": [{
            "index": index,
            "id": id,
            "type": "function",
            "function": {"name": item["name"].clone(), "arguments": ""}
        }]}, "finish_reason": null}))
    }

    fn arguments_chunk(index: u64, arguments: &str) -> Bytes {
        Self::chunk(json!({"index": 0, "delta": {"tool_calls": [{
            "index": index,
            "function": {"arguments": arguments}
        }]}, "finish_reason": null}))
    }

    /// Translate a single SSE line. Returns the OpenAI-shaped bytes to
    /// forward, or `None` for lines that carry nothing the client needs
    /// (`event:` names, reasoning summaries, item boundaries, pings).
    pub fn translate_line(&mut self, line: &str) -> Option<Bytes> {
        if self.finished {
            return None;
        }
        let json_str = line.strip_prefix("data: ").or_else(|| line.strip_prefix("data:"))?;
        let v: Value = serde_json::from_str(json_str.trim()).ok()?;
        match v["type"].as_str()? {
            "response.output_item.added" => {
                let item = &v["item"];
                if item["type"] != "function_call" {
                    return None;
                }
                Some(self.open_call(v["output_index"].as_u64().unwrap_or(0), item))
            }
            "response.function_call_arguments.delta" => {
                let index = *self.tool_indices.get(&v["output_index"].as_u64().unwrap_or(0))?;
                self.streamed_arguments.insert(index);
                Some(Self::arguments_chunk(index, v["delta"].as_str()?))
            }
            "response.function_call_arguments.done" => {
                let index = *self.tool_indices.get(&v["output_index"].as_u64().unwrap_or(0))?;
                if self.streamed_arguments.contains(&index) {
                    return None;
                }
                self.streamed_arguments.insert(index);
                Some(Self::arguments_chunk(index, &arguments_string(&v["arguments"])))
            }
            "response.output_item.done" => {
                // A server that never announced the item: emit the whole call.
                let item = &v["item"];
                let output_index = v["output_index"].as_u64().unwrap_or(0);
                if item["type"] != "function_call" || self.tool_indices.contains_key(&output_index) {
                    return None;
                }
                let header = self.open_call(output_index, item);
                let index = self.tool_indices[&output_index];
                self.streamed_arguments.insert(index);
                let mut out = header.to_vec();
                out.extend_from_slice(&Self::arguments_chunk(index, &arguments_string(&item["arguments"])));
                Some(Bytes::from(out))
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let text = v["delta"].as_str()?;
                Some(Self::chunk(json!({"index": 0, "delta": {"content": text}, "finish_reason": null})))
            }
            "response.completed" | "response.incomplete" => {
                self.finished = true;
                let response = &v["response"];
                let reason = finish_reason(response, self.next_tool_index > 0);
                let mut chunk = json!({
                    "id": "chatcmpl-stream",
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]
                });
                if let Some(usage) = chat_usage(&response["usage"]) {
                    chunk["usage"] = usage;
                }
                Some(Bytes::from(format!("data: {chunk}\n\ndata: [DONE]\n\n")))
            }
            "response.failed" | "error" => {
                self.finished = true;
                let message = if v["type"] == "error" {
                    v["message"].as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
                } else {
                    failure_message(&v["response"])
                };
                let chunk = json!({"error": {"message": format!("Azure AI Foundry reported the response as failed: {message}")}});
                Some(Bytes::from(format!("data: {chunk}\n\ndata: [DONE]\n\n")))
            }
            _ => None,
        }
    }
}

/// Translate a Responses event-stream body into OpenAI-compatible chunks.
pub(crate) fn translate_sse_stream(
    body: impl futures::Stream<Item = anyhow::Result<Bytes>> + Send + 'static,
) -> SseStream {
    let mut translator = ResponsesSseTranslator::new();
    // An event split across body chunks must be translated whole, not
    // dropped half by half; see `SseLineBuffer`.
    let mut lines = crate::providers::sse_lines::SseLineBuffer::new();
    Box::pin(body.map_ok(move |chunk| {
        let mut out = Vec::new();
        for line in lines.push(&chunk) {
            if let Some(translated) = translator.translate_line(&line) {
                out.extend_from_slice(&translated);
            }
        }
        Bytes::from(out)
    }))
}

/// Post `req` to the Responses URL with the adapter's credential; a non-2xx is
/// the adapter's own error (the same 401/404 hints as chat completions).
async fn send(adapter: &FoundryAdapter, req: &NormalizedRequest, stream: bool) -> anyhow::Result<reqwest::Response> {
    let url = adapter.endpoint().responses_url();
    let body = build_body(req, stream);
    tracing::debug!(
        url = %url,
        surface = adapter.endpoint().surface().label(),
        auth = adapter.auth().label(),
        model = %req.model,
        stream,
        "foundry: tool-carrying request over the Responses API"
    );
    let request = adapter
        .auth()
        .apply(adapter.http_client().post(&url).timeout(adapter.request_timeout(req)).json(&body))
        .await
        .context("failed to attach credentials to the Azure AI Foundry request")?;
    let resp = request
        .send()
        .await
        .with_context(|| format!("failed to send request to Azure AI Foundry at {url}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let text = resp.text().await.unwrap_or_default();
    tracing::warn!(url = %url, %status, model = %req.model, body = text.as_str(), "foundry: Responses request failed");
    Err(adapter.http_error(&req.model, status, &text))
}

/// Non-streamed completion over the Responses API.
pub(crate) async fn complete(adapter: &FoundryAdapter, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
    let dispatched = std::time::Instant::now();
    let resp = send(adapter, req, false).await?;
    let ttft_ms = dispatched.elapsed().as_millis() as i64;
    let v: Value = resp
        .json()
        .await
        .context("Azure AI Foundry returned a Responses body that is not JSON")?;
    let mut result = parse_response(v)?;
    result.ttft_ms = Some(ttft_ms);
    tracing::debug!(
        model = %req.model,
        prompt_tokens = result.prompt_tokens,
        completion_tokens = result.completion_tokens,
        cached_tokens = result.cache_read_tokens,
        finish_reason = %result.finish_reason,
        ttft_ms,
        "foundry: Responses completion ok"
    );
    Ok(result)
}

/// Streamed completion over the Responses API, translated to OpenAI chunks.
pub(crate) async fn stream(adapter: &FoundryAdapter, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
    let resp = send(adapter, req, true).await?;
    Ok(translate_sse_stream(
        resp.bytes_stream()
            .map_err(|e| anyhow::anyhow!("Azure AI Foundry stream error: {}", e)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::adapter::ReasoningControl;

    fn req() -> NormalizedRequest {
        NormalizedRequest {
            model: "gpt-6-deployment".into(),
            request_model: "deep".into(),
            messages: vec![
                json!({"role": "system", "content": "be brief"}),
                json!({"role": "user", "content": "revenue?"}),
            ],
            temperature: None,
            max_tokens: Some(4096),
            tools: Some(vec![json!({
                "type": "function",
                "function": {
                    "name": "lookup",
                    "description": "look a figure up",
                    "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}
                }
            })]),
            tool_choice: Some(json!("auto")),
            reasoning: Some(ReasoningControl { disable_thinking: false, effort: Some("high") }),
            ..Default::default()
        }
    }

    #[test]
    fn the_body_names_the_deployment_and_carries_the_responses_fields() {
        let body = build_body(&req(), false);
        assert_eq!(body["model"], "gpt-6-deployment");
        assert_eq!(body["stream"], false);
        assert_eq!(body["store"], false);
        assert_eq!(body["max_output_tokens"], 4096);
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["reasoning"]["effort"], "high");
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("messages").is_none());
        assert_eq!(body["input"][0]["role"], "system");
        assert_eq!(body["input"][0]["content"], "be brief");
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(build_body(&req(), true)["stream"], true);
    }

    #[test]
    fn chat_tools_flatten_to_responses_tools_with_the_chat_strict_default() {
        let body = build_body(&req(), false);
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "lookup");
        assert_eq!(tool["description"], "look a figure up");
        assert_eq!(tool["parameters"]["properties"]["q"]["type"], "string");
        assert_eq!(tool["strict"], false, "the Responses default is strict; chat callers wrote non-strict schemas");
        assert!(tool.get("function").is_none());
        assert_eq!(body["tool_choice"], "auto");

        let mut r = req();
        r.tools = Some(vec![json!({"type": "function", "function": {"name": "f", "strict": true}})]);
        r.tool_choice = Some(json!({"type": "function", "function": {"name": "f"}}));
        let body = build_body(&r, false);
        assert_eq!(body["tools"][0]["strict"], true);
        assert_eq!(body["tool_choice"], json!({"type": "function", "name": "f"}));

        r.tools = Some(vec![json!({"type": "function", "name": "already-flat"})]);
        assert_eq!(build_body(&r, false)["tools"][0], json!({"type": "function", "name": "already-flat"}));
    }

    #[test]
    fn no_tools_sends_no_tool_fields_and_no_effort_for_a_non_reasoning_model() {
        let mut r = req();
        r.tools = None;
        r.model = "some-other-deployment".into();
        let body = build_body(&r, false);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("reasoning").is_none());
        r.tools = Some(vec![]);
        assert!(build_body(&r, false).get("tools").is_none());
    }

    #[test]
    fn an_agentic_turn_becomes_function_call_and_output_items() {
        let mut r = req();
        r.messages = vec![
            json!({"role": "user", "content": "revenue?"}),
            json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": "{\"q\":\"revenue\"}"}},
                {"id": "call_2", "type": "function", "function": {"name": "lookup", "arguments": {"q": "margin"}}}
            ]}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "12"}),
            json!({"role": "tool", "tool_call_id": "call_2", "content": [{"type": "text", "text": "0.3"}]}),
            json!({"role": "assistant", "content": "Revenue is 12."}),
        ];
        let input = build_body(&r, false)["input"].clone();
        assert_eq!(input[0], json!({"role": "user", "content": "revenue?"}));
        assert_eq!(input[1], json!({"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"revenue\"}"}));
        assert_eq!(input[2]["arguments"], "{\"q\":\"margin\"}", "object arguments are serialised");
        assert_eq!(input[3], json!({"type": "function_call_output", "call_id": "call_1", "output": "12"}));
        assert_eq!(input[4], json!({"type": "function_call_output", "call_id": "call_2", "output": "0.3"}));
        assert_eq!(input[5], json!({"role": "assistant", "content": "Revenue is 12."}));
        assert_eq!(input.as_array().unwrap().len(), 6, "an assistant turn with null content adds no message item");
    }

    #[test]
    fn multipart_user_content_becomes_input_parts() {
        let mut r = req();
        r.messages = vec![json!({"role": "user", "content": [
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "https://example.test/a.png", "detail": "low"}},
            {"type": "input_text", "text": "already responses-shaped"}
        ]})];
        let content = build_body(&r, false)["input"][0]["content"].clone();
        assert_eq!(content[0], json!({"type": "input_text", "text": "what is this?"}));
        assert_eq!(content[1], json!({"type": "input_image", "image_url": "https://example.test/a.png", "detail": "low"}));
        assert_eq!(content[2], json!({"type": "input_text", "text": "already responses-shaped"}));
    }

    #[test]
    fn response_format_becomes_text_format() {
        let mut r = req();
        r.response_format = Some(json!({"type": "json_object"}));
        assert_eq!(build_body(&r, false)["text"]["format"], json!({"type": "json_object"}));
        r.response_format = Some(json!({"type": "json_schema", "json_schema": {
            "name": "answer", "strict": true, "schema": {"type": "object"}
        }}));
        let format = build_body(&r, false)["text"]["format"].clone();
        assert_eq!(format, json!({"type": "json_schema", "name": "answer", "strict": true, "schema": {"type": "object"}}));
        r.response_format = None;
        assert!(build_body(&r, false).get("text").is_none());
    }

    #[test]
    fn a_completed_response_parses_text_usage_and_cache_reads() {
        let result = parse_response(json!({
            "id": "resp_1", "object": "response", "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_1", "summary": []},
                {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                 "content": [{"type": "output_text", "text": "Revenue ", "annotations": []},
                             {"type": "output_text", "text": "is 12.", "annotations": []}]}
            ],
            "usage": {"input_tokens": 40, "output_tokens": 90, "total_tokens": 130,
                      "input_tokens_details": {"cached_tokens": 16},
                      "output_tokens_details": {"reasoning_tokens": 80}}
        }))
        .unwrap();
        assert_eq!(result.content, "Revenue is 12.");
        assert_eq!(result.prompt_tokens, 40);
        assert_eq!(result.completion_tokens, 90);
        assert_eq!(result.cache_read_tokens, 16);
        assert_eq!(result.reasoning_tokens, Some(80));
        assert_eq!(result.finish_reason, "stop");
        assert!(result.tool_calls.is_none());
        assert!(result.is_cached());
    }

    #[test]
    fn function_call_items_parse_back_as_chat_tool_calls_keyed_by_call_id() {
        let result = parse_response(json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_1", "summary": []},
                {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup",
                 "arguments": "{\"q\":\"revenue\"}", "status": "completed"},
                {"type": "function_call", "id": "fc_2", "call_id": "call_2", "name": "lookup",
                 "arguments": "{\"q\":\"margin\"}", "status": "completed"}
            ],
            "usage": {"input_tokens": 20, "output_tokens": 7}
        }))
        .unwrap();
        assert_eq!(result.finish_reason, "tool_calls");
        assert_eq!(result.content, "");
        let calls = result.tool_calls.expect("the calls");
        assert_eq!(calls[0], json!({"id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": "{\"q\":\"revenue\"}"}}));
        assert_eq!(calls[1]["id"], "call_2");
        assert_eq!(result.prompt_tokens, 20);
        assert_eq!(result.cache_read_tokens, 0);
    }

    #[test]
    fn an_incomplete_response_cut_at_max_output_tokens_is_finish_reason_length() {
        let result = parse_response(json!({
            "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Reven"}]}],
            "usage": {"input_tokens": 1, "output_tokens": 2}
        }))
        .unwrap();
        assert_eq!(result.finish_reason, "length");
        assert_eq!(result.content, "Reven");
    }

    #[test]
    fn a_failed_response_is_an_error_naming_the_cause_and_missing_usage_meters_zero() {
        let err = parse_response(json!({"status": "failed", "error": {"code": "server_error", "message": "boom"}}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom"), "{err}");
        let empty = parse_response(json!({"status": "completed", "output": []})).unwrap();
        assert_eq!(empty.content, "");
        assert_eq!(empty.prompt_tokens, 0);
        assert_eq!(empty.finish_reason, "stop");
        assert!(parse_response(json!("nope")).is_err());
    }

    fn translate_all(lines: &[&str]) -> Vec<Value> {
        let mut t = ResponsesSseTranslator::new();
        let mut out = String::new();
        for line in lines {
            if let Some(b) = t.translate_line(line) {
                out.push_str(std::str::from_utf8(&b).unwrap());
            }
        }
        out.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .map(|d| if d == "[DONE]" { json!("[DONE]") } else { serde_json::from_str(d).unwrap() })
            .collect()
    }

    #[test]
    fn a_streamed_tool_call_becomes_a_header_chunk_argument_fragments_and_a_final_chunk_with_usage() {
        let chunks = translate_all(&[
            "event: response.created",
            r#"data: {"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}"#,
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
            r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":""}}"#,
            r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_1","delta":"{\"q\":"}"#,
            r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_1","delta":"\"revenue\"}"}"#,
            r#"data: {"type":"response.function_call_arguments.done","output_index":1,"item_id":"fc_1","arguments":"{\"q\":\"revenue\"}"}"#,
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{\"q\":\"revenue\"}"}}"#,
            r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":20,"output_tokens":7,"input_tokens_details":{"cached_tokens":4},"output_tokens_details":{"reasoning_tokens":5}}}}"#,
        ]);
        assert_eq!(chunks.len(), 5, "{chunks:?}");
        let header = &chunks[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(header["index"], 0, "the reasoning item at output_index 0 takes no tool index");
        assert_eq!(header["id"], "call_1");
        assert_eq!(header["type"], "function");
        assert_eq!(header["function"]["name"], "lookup");
        assert_eq!(header["function"]["arguments"], "");
        assert_eq!(chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{\"q\":");
        assert_eq!(chunks[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "\"revenue\"}");
        assert_eq!(chunks[2]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        let last = &chunks[3];
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(last["usage"]["prompt_tokens"], 20);
        assert_eq!(last["usage"]["completion_tokens"], 7);
        assert_eq!(last["usage"]["total_tokens"], 27);
        assert_eq!(last["usage"]["prompt_tokens_details"]["cached_tokens"], 4);
        assert_eq!(last["usage"]["completion_tokens_details"]["reasoning_tokens"], 5);
        assert_eq!(chunks[4], json!("[DONE]"));
        for c in &chunks[..4] {
            assert_eq!(c["object"], "chat.completion.chunk");
        }
    }

    #[test]
    fn text_deltas_become_content_chunks_and_an_incomplete_stream_ends_with_length() {
        let chunks = translate_all(&[
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}"#,
            r#"data: {"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}"#,
            r#"data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"Reve"}"#,
            r#"data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"nue"}"#,
            r#"data: {"type":"response.output_text.done","output_index":0,"content_index":0,"text":"Revenue"}"#,
            r#"data: {"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":3,"output_tokens":2}}}"#,
            r#"data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"late"}"#,
        ]);
        assert_eq!(chunks.len(), 4, "{chunks:?}");
        assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "Reve");
        assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "nue");
        assert_eq!(chunks[2]["choices"][0]["finish_reason"], "length");
        assert_eq!(chunks[2]["usage"]["prompt_tokens"], 3);
        assert_eq!(chunks[3], json!("[DONE]"));
    }

    #[test]
    fn arguments_delivered_only_in_the_done_event_are_emitted_once() {
        let chunks = translate_all(&[
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"lookup","arguments":""}}"#,
            r#"data: {"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"q\":1}"}"#,
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":1}"}}"#,
            r#"data: {"type":"response.completed","response":{"status":"completed"}}"#,
        ]);
        assert_eq!(chunks.len(), 4, "{chunks:?}");
        assert_eq!(chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{\"q\":1}");
        assert!(chunks[2].get("usage").is_none(), "no usage reported, none invented");
        assert_eq!(chunks[2]["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn an_item_never_announced_is_emitted_whole_from_its_done_event() {
        let chunks = translate_all(&[
            r#"data: {"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","call_id":"call_9","name":"lookup","arguments":"{}"}}"#,
            r#"data: {"type":"response.completed","response":{"status":"completed"}}"#,
        ]);
        assert_eq!(chunks.len(), 4, "{chunks:?}");
        assert_eq!(chunks[0]["choices"][0]["delta"]["tool_calls"][0]["id"], "call_9");
        assert_eq!(chunks[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{}");
    }

    #[test]
    fn a_failed_stream_surfaces_an_error_chunk_then_done() {
        let chunks = translate_all(&[
            r#"data: {"type":"response.output_text.delta","output_index":0,"delta":"x"}"#,
            r#"data: {"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"boom"}}}"#,
        ]);
        assert_eq!(chunks.len(), 3, "{chunks:?}");
        assert!(chunks[1]["error"]["message"].as_str().unwrap().contains("boom"));
        assert_eq!(chunks[2], json!("[DONE]"));
        let chunks = translate_all(&[r#"data: {"type":"error","code":"rate_limit","message":"slow down"}"#]);
        assert!(chunks[0]["error"]["message"].as_str().unwrap().contains("slow down"));
    }

    #[tokio::test]
    async fn the_stream_wrapper_reassembles_events_split_across_body_chunks() {
        use futures::StreamExt;
        let event = r#"data: {"type":"response.output_text.delta","output_index":0,"delta":"héllo"}"#;
        let bytes = format!("{event}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\"}}}}\n\n");
        let (a, b) = bytes.as_bytes().split_at(event.len() / 2 + 1);
        let body = futures::stream::iter(vec![Ok(Bytes::copy_from_slice(a)), Ok(Bytes::copy_from_slice(b))]);
        let mut out = Vec::new();
        let mut s = translate_sse_stream(body);
        while let Some(chunk) = s.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"content\":\"héllo\""), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    }
}
