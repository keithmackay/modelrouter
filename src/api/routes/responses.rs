use std::time::Instant;

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;
use tracing::Instrument;

use super::tools::{
    assistant_message, ensure_tools_supported, forwarded_tools, validate_tool_choice_requires_tools,
};
use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::models::{NewCostLedgerEntry, NewPrompt},
    providers::adapter::NormalizedRequest,
    router::policy::PolicyDecision,
};

pub async fn responses_handler(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!(
        "responses_handler",
        user_id = tracing::field::Empty,
        model = tracing::field::Empty,
    );
    responses_inner(State(state), user, headers, Json(body))
        .instrument(span)
        .await
}

async fn responses_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    crate::api::routes::reject_experiment_header("/v1/responses", &headers)?;
    let user = user.0;
    tracing::Span::current().record("user_id", user.id);
    let attribution = crate::api::attribution::Attribution::extract(&body, &headers)?;
    let attr_correlation = attribution.correlation_id.clone();
    let attr_tags = attribution.tags_json();

    let requested_model = body["model"]
        .as_str()
        .unwrap_or(&state.settings.routing.default_model)
        .to_string();
    // A scoped alias override for the request's attribution tags pins the model.
    let model = state
        .router
        .scoped_name(&attribution.tags, &requested_model)
        .unwrap_or(requested_model);

    tracing::Span::current().record("model", model.as_str());

    // Policy check
    let policy_result = state
        .policy
        .check(&user, &model)
        .instrument(tracing::info_span!("modelrouter.policy_check"))
        .await
        .map_err(|_| ApiError::Internal)?;
    let _concurrency_permit = match policy_result {
        PolicyDecision::Allow { max_concurrent } => {
            if let Some(max) = max_concurrent {
                match state.concurrency.try_acquire(user.id, max) {
                    Some(permit) => Some(permit),
                    None => {
                        return Err(ApiError::PolicyDenied {
                            reason: "concurrent request limit exceeded".to_string(),
                            status: 429,
                        })
                    }
                }
            } else {
                None
            }
        }
        PolicyDecision::Deny { reason, status, .. } => {
            return Err(ApiError::PolicyDenied { reason, status });
        }
    };

    // Route the model
    let (provider_name, canonical_model) = state.router.resolve(&model);

    // Operator disable gate (issue #5) — 403 naming the reason, never a provider call.
    state
        .router
        .check_available(&provider_name, &canonical_model)?;

    // Tools (issue #87). Function tools are forwarded to tool-capable
    // backends exactly as on /v1/chat/completions (issue #88); any other tool
    // type — hosted web/grounded search in particular — is refused with a
    // pointer to /v1/search rather than silently dropped.
    reject_non_function_tools(&body)?;
    validate_tool_choice_requires_tools(&body)?;
    ensure_tools_supported(&state, &body, &provider_name, &canonical_model)?;

    // Translate body: if messages absent and input is a string, synthesize messages
    let mut body = body;
    let has_messages = body["messages"].is_array();
    if !has_messages {
        if let Some(input_str) = body["input"].as_str() {
            let messages = serde_json::json!([{"role": "user", "content": input_str}]);
            body["messages"] = messages;
        } else if body["input"].is_array() {
            // Array-form input: message items pass through; function-call
            // items become chat-completions tool turns.
            let items = body["input"].as_array().cloned().unwrap_or_default();
            body["messages"] = Value::Array(input_items_to_messages(&items));
            body.as_object_mut().map(|m| m.remove("input"));
        }
    }
    // Remove "input" key
    if let Some(obj) = body.as_object_mut() {
        obj.remove("input");
    }

    // Same capability filter as /v1/chat/completions — this route resolves the
    // same aliases through the same router, so it reaches the same models that
    // reject `temperature` and would 400 for the same reason.
    let temperature = body["temperature"].as_f64().filter(|_| {
        crate::router::model_capabilities::temperature_allowed(
            &canonical_model,
            &state.settings.model_capabilities,
            state.router.learned_capabilities(),
        )
    });

    normalize_function_tools(&mut body);
    let (tools, tool_choice) = forwarded_tools(&body);

    let norm_req = NormalizedRequest {
        model: canonical_model.clone(),
        request_model: model.clone(),
        messages: body["messages"].as_array().cloned().unwrap_or_default(),
        stream: false,
        temperature,
        max_tokens: crate::router::model_capabilities::clamp_max_tokens(
            &canonical_model,
            body["max_tokens"].as_u64().map(|v| v as u32),
            &state.settings.model_capabilities,
        ),
        tools,
        tool_choice,
        // The Responses API nests the level as `reasoning.effort`; accept the
        // chat-completions spelling too.
        reasoning: crate::router::model_capabilities::resolve_reasoning(
            &canonical_model,
            body["reasoning"]["effort"]
                .as_str()
                .or_else(|| body["reasoning_effort"].as_str()),
            &state.settings.model_capabilities,
        ),
        extra_params: serde_json::Value::Object(Default::default()),
    };

    let start = Instant::now();

    // Check circuit breaker before calling provider
    if state.circuit_breaker.is_open(&provider_name) {
        return Err(ApiError::ProviderError(anyhow::anyhow!(
            "{provider_name} is circuit-broken"
        )));
    }

    let adapter = state
        .provider_registry
        .get(&provider_name)
        .map_err(ApiError::ProviderError)?;
    let first_try = adapter.complete(&norm_req).await;
    let result = match first_try {
        Err(e) => match crate::router::model_capabilities::temperature_rejection_retry(&norm_req, &e) {
            Some(retry) => {
                crate::router::learned_capabilities::record_temperature_rejection(
                    state.router.learned_capabilities(),
                    &*state.db,
                    &norm_req.model,
                    &e,
                )
                .await;
                adapter.complete(&retry).await
            }
            None => Err(e),
        },
        ok => ok,
    };
    let result = result.map_err(|e| {
        state
            .circuit_breaker
            .record_provider_failure(&provider_name, &e);
        ApiError::ProviderError(e)
    })?;
    state.circuit_breaker.record_success(&provider_name);

    let latency_ms = start.elapsed().as_millis() as i64;

    let cost = state.cost_calc.calculate_with_cache(
        &canonical_model,
        result.prompt_tokens,
        result.completion_tokens,
        result.cache_read_tokens,
        result.cache_write_tokens,
    );

    // Fire-and-forget cost logging
    record_responses_cost(
        state.clone(),
        user.id,
        user.api_key_id,
        attribution.project_or(user.api_key_project.clone()),
        model.clone(),
        canonical_model.clone(),
        provider_name.clone(),
        body["messages"].as_array().cloned().unwrap_or_default(),
        result.content.clone(),
        result.finish_reason.clone(),
        result.prompt_tokens,
        result.completion_tokens,
        result.cache_read_tokens,
        result.cache_write_tokens,
        cost,
        latency_ms,
        attr_correlation,
        attr_tags,
    );

    let response_body = serde_json::json!({
        "id": format!("resp_{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()),
        "object": "response",
        "model": canonical_model,
        "choices": [{
            "message": assistant_message(&result),
            "finish_reason": result.finish_reason
        }],
        "usage": {
            "input_tokens": result.prompt_tokens,
            "output_tokens": result.completion_tokens
        }
    });

    Ok(Json(response_body).into_response())
}

#[allow(clippy::too_many_arguments)]
fn record_responses_cost(
    state: AppState,
    user_id: i64,
    api_key_id: Option<i64>,
    user_project: Option<String>,
    request_model: String,
    routed_model: String,
    provider: String,
    messages: Vec<Value>,
    response: String,
    finish_reason: String,
    prompt_tokens: u32,
    completion_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
    cost: f64,
    latency_ms: i64,
    attr_correlation: Option<String>,
    attr_tags: String,
) {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    tokio::spawn(async move {
        let messages_json = serde_json::to_string(&messages).unwrap_or_default();
        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model,
            routed_model: routed_model.clone(),
            provider: provider.clone(),
            messages: messages_json,
            response: Some(response),
            finish_reason: Some(finish_reason),
            prompt_tokens: prompt_tokens as i64,
            completion_tokens: completion_tokens as i64,
            cache_read_tokens: cache_read_tokens as i64,
            cache_write_tokens: cache_write_tokens as i64,
            cost_usd: cost,
            latency_ms: Some(latency_ms),
            ttft_ms: None,
            attempts: None,
            tags: "[]".to_string(),
            project: user_project.clone(),
            attribution_correlation_id: attr_correlation.clone(),
            attribution_tags: attr_tags.clone(),
            experiment_id: None,
            experiment_variant: None,
        };
        // Storage policy (issue #4): the prompt row is optional; the cost row is not.
        let stored =
            match crate::db::prompt_store::apply_storage_policy(&state.storage.load(), prompt) {
                Some(p) => match PromptRepository::create(&*state.prompt_db, p).await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::error!("Failed to record responses prompt: {e}");
                        None
                    }
                },
                None => None,
            };
        {
            let ledger = NewCostLedgerEntry {
                user_id,
                prompt_id: stored.as_ref().map(|s| s.id),
                model: routed_model,
                provider,
                project: user_project,
                tokens_in: prompt_tokens as i64,
                tokens_out: completion_tokens as i64,
                cost_usd: cost,
                api_key_id,
                attribution_correlation_id: attr_correlation,
                attribution_tags: attr_tags,
                experiment_id: None,
                experiment_variant: None,
                tokens_estimated: false,
            };
            let _ = CostRepository::create(&*state.db, ledger).await;
        }
    });
}

/// Refuse any `tools` entry (or object `tool_choice`) that is not a function
/// tool. Hosted tools such as web search or grounding cannot be forwarded to
/// an arbitrary backend; grounded search has its own endpoint.
fn reject_non_function_tools(body: &Value) -> Result<(), ApiError> {
    let tool_type = |v: &Value| v["type"].as_str().unwrap_or("<missing>").to_string();
    if let Some(tools) = body["tools"].as_array() {
        if let Some(bad) = tools.iter().find(|t| t["type"] != "function") {
            return Err(ApiError::InvalidRequest(format!(
                "`tools` entry of type '{}' is not supported: only function tools are forwarded; grounded search belongs on /v1/search",
                tool_type(bad)
            )));
        }
    }
    if let Some(tc) = body.get("tool_choice").filter(|tc| tc.is_object()) {
        if tc["type"] != "function" {
            return Err(ApiError::InvalidRequest(format!(
                "`tool_choice` of type '{}' is not supported: only function tools are forwarded; grounded search belongs on /v1/search",
                tool_type(tc)
            )));
        }
    }
    Ok(())
}

/// Responses-API function tools are flat (`{"type":"function","name",...}`);
/// the adapters take the chat-completions shape with the definition nested
/// under `function`. Rewrite flat entries (and a flat named `tool_choice`) in
/// place; already-nested entries are left alone, so both shapes are accepted.
fn normalize_function_tools(body: &mut Value) {
    if let Some(tools) = body["tools"].as_array_mut() {
        for tool in tools.iter_mut() {
            if tool["function"].is_object() {
                continue;
            }
            let mut function = serde_json::Map::new();
            for key in ["name", "description", "parameters", "strict"] {
                if let Some(v) = tool.get(key).filter(|v| !v.is_null()) {
                    function.insert(key.to_string(), v.clone());
                }
            }
            *tool = serde_json::json!({"type": "function", "function": function});
        }
    }
    if let Some(tc) = body.get_mut("tool_choice") {
        if tc.is_object() && !tc["function"].is_object() {
            if let Some(name) = tc["name"].as_str() {
                *tc = serde_json::json!({"type": "function", "function": {"name": name}});
            }
        }
    }
}

/// Map Responses-API input items onto chat-completions messages.
///
/// - `function_call` items become `tool_calls` on an assistant turn, merged
///   into the preceding assistant message when there is one (a model turn
///   that spoke and called tools is a single response).
/// - `function_call_output` items become `tool`-role messages.
/// - Everything else passes through unchanged.
fn input_items_to_messages(items: &[Value]) -> Vec<Value> {
    let as_string = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let mut messages: Vec<Value> = Vec::with_capacity(items.len());
    for item in items {
        match item["type"].as_str() {
            Some("function_call") => {
                let call = serde_json::json!({
                    "id": item["call_id"].clone(),
                    "type": "function",
                    "function": {
                        "name": item["name"].clone(),
                        "arguments": as_string(&item["arguments"]),
                    },
                });
                match messages.last_mut().filter(|m| m["role"] == "assistant") {
                    Some(prev) => match prev["tool_calls"].as_array_mut() {
                        Some(calls) => calls.push(call),
                        None => prev["tool_calls"] = Value::Array(vec![call]),
                    },
                    None => messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": Value::Null,
                        "tool_calls": [call],
                    })),
                }
            }
            Some("function_call_output") => messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": item["call_id"].clone(),
                "content": as_string(&item["output"]),
            })),
            _ => messages.push(item.clone()),
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn function_tools_pass_the_type_check() {
        let body = json!({
            "tools": [{"type": "function", "name": "f"}],
            "tool_choice": {"type": "function", "name": "f"},
        });
        assert!(reject_non_function_tools(&body).is_ok());
        assert!(reject_non_function_tools(&json!({"tool_choice": "auto"})).is_ok());
    }

    #[test]
    fn hosted_search_tools_are_refused_with_a_pointer_to_search() {
        for ty in ["web_search_preview", "web_search", "file_search"] {
            let body = json!({"tools": [{"type": "function", "name": "f"}, {"type": ty}]});
            let err = reject_non_function_tools(&body).unwrap_err();
            let msg = match err {
                ApiError::InvalidRequest(m) => m,
                other => panic!("expected InvalidRequest, got {other:?}"),
            };
            assert!(msg.contains(ty), "names the offending type: {msg}");
            assert!(msg.contains("/v1/search"), "points at /v1/search: {msg}");
        }
        let body = json!({"tools": [{"name": "untyped"}]});
        assert!(reject_non_function_tools(&body).is_err());
    }

    #[test]
    fn hosted_tool_choice_is_refused() {
        let body = json!({
            "tools": [{"type": "function", "name": "f"}],
            "tool_choice": {"type": "web_search_preview"},
        });
        assert!(matches!(
            reject_non_function_tools(&body),
            Err(ApiError::InvalidRequest(m)) if m.contains("tool_choice") && m.contains("/v1/search")
        ));
    }

    #[test]
    fn flat_function_tools_are_nested_for_the_adapters() {
        let mut body = json!({
            "tools": [
                {"type": "function", "name": "get_weather", "description": "d",
                 "parameters": {"type": "object"}, "strict": true},
                {"type": "function", "function": {"name": "already_nested"}},
            ],
            "tool_choice": {"type": "function", "name": "get_weather"},
        });
        normalize_function_tools(&mut body);
        assert_eq!(
            body["tools"][0],
            json!({"type": "function", "function": {
                "name": "get_weather", "description": "d",
                "parameters": {"type": "object"}, "strict": true}})
        );
        assert_eq!(body["tools"][1]["function"]["name"], "already_nested");
        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "function": {"name": "get_weather"}})
        );
    }

    #[test]
    fn string_tool_choice_is_untouched() {
        let mut body = json!({"tools": [], "tool_choice": "required"});
        normalize_function_tools(&mut body);
        assert_eq!(body["tool_choice"], "required");
    }

    #[test]
    fn function_call_items_become_chat_tool_turns() {
        let items = vec![
            json!({"role": "user", "content": "weather?"}),
            json!({"type": "function_call", "call_id": "c1", "name": "get_weather",
                   "arguments": "{\"city\":\"Paris\"}"}),
            json!({"type": "function_call", "call_id": "c2", "name": "get_time",
                   "arguments": {"tz": "CET"}}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "sunny"}),
            json!({"type": "function_call_output", "call_id": "c2", "output": {"t": "12:00"}}),
        ];
        let m = input_items_to_messages(&items);
        assert_eq!(m.len(), 4);
        assert_eq!(m[0], items[0]);
        assert_eq!(m[1]["role"], "assistant");
        assert!(m[1]["content"].is_null());
        let calls = m[1]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2, "consecutive calls share one assistant turn");
        assert_eq!(calls[0]["id"], "c1");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(calls[0]["function"]["arguments"], "{\"city\":\"Paris\"}");
        assert_eq!(
            calls[1]["function"]["arguments"], "{\"tz\":\"CET\"}",
            "object arguments serialize"
        );
        assert_eq!(
            m[2],
            json!({"role": "tool", "tool_call_id": "c1", "content": "sunny"})
        );
        assert_eq!(m[3]["content"], "{\"t\":\"12:00\"}");
    }

    #[test]
    fn function_call_after_assistant_text_joins_that_turn() {
        let items = vec![
            json!({"role": "assistant", "content": "Let me check."}),
            json!({"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"}),
        ];
        let m = input_items_to_messages(&items);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["content"], "Let me check.");
        assert_eq!(m[0]["tool_calls"][0]["id"], "c1");
    }
}
