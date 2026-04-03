use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::{extract::State, response::{IntoResponse, Response}, Json};
use serde_json::Value;
use tracing::Instrument;

use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::{
        models::{NewCostLedgerEntry, NewPrompt},
    },
    router::policy::PolicyDecision,
};

pub async fn chat_completions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!(
        "chat_completions",
        user_id = tracing::field::Empty,
        model = tracing::field::Empty,
        provider = tracing::field::Empty,
        streaming = tracing::field::Empty,
        "cost.usd" = tracing::field::Empty,
        "tokens.prompt" = tracing::field::Empty,
    );
    chat_completions_inner(State(state), user, Json(body))
        .instrument(span)
        .await
}

async fn chat_completions_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;
    let model = body["model"]
        .as_str()
        .unwrap_or(&state.settings.routing.default_model)
        .to_string();
    let stream = body["stream"].as_bool().unwrap_or(false);

    // Fire on_request_received lifecycle hooks
    for hook in &state.settings.hooks.lifecycle {
        if hook.event == "on_request_received" {
            let payload = crate::hooks::lifecycle::request_received_payload(
                &user.name,
                &model,
                body["messages"].as_array().map(|m| m.len()).unwrap_or(0),
            );
            crate::hooks::lifecycle::fire(hook, payload);
        }
    }

    // Policy check
    let policy_result = state
        .policy
        .check(&user, &model)
        .instrument(tracing::info_span!("modelrouter.policy_check"))
        .await
        .map_err(|_| ApiError::Internal)?;
    match policy_result {
        PolicyDecision::Allow => {}
        PolicyDecision::Deny {
            reason,
            status,
            budget_context,
        } => {
            // Only fire on_budget_exceeded if this is actually a budget denial (has budget context)
            if budget_context.is_some() {
                for hook in &state.settings.hooks.lifecycle {
                    if hook.event == "on_budget_exceeded" {
                        let ctx = budget_context.as_ref();
                        let payload = crate::hooks::lifecycle::budget_exceeded_payload(
                            &user.name,
                            &model,
                            ctx.map(|c| c.limit_usd).unwrap_or(0.0),
                            ctx.map(|c| c.spent_usd).unwrap_or(0.0),
                            ctx.map(|c| c.window.as_str()).unwrap_or("unknown"),
                        );
                        crate::hooks::lifecycle::fire(hook, payload);
                    }
                }
            }
            #[cfg(feature = "otel")]
            {
                let metric_reason = match reason.as_str() {
                    r if r.contains("budget") => "budget",
                    r if r.contains("rate") => "rate_limit",
                    _ => "model_denied",
                };
                crate::telemetry::metrics::record_request(
                    &model, &state.router.resolve(&model).0, "policy_denied",
                );
                crate::telemetry::metrics::record_policy_denied(metric_reason);
            }
            return Err(ApiError::PolicyDenied { reason, status });
        }
    }

    // Run pre_request pipeline hooks (may mutate body)
    let body = crate::hooks::pipeline::run_pre_request(
        &state.settings.hooks.pipeline,
        &state.db,
        body,
    )
    .await
    .map_err(|_| ApiError::Internal)?;

    let (provider_name, canonical_model) = state.router.resolve(&model);

    let span = tracing::Span::current();
    span.record("user_id", user.id);
    span.record("model", canonical_model.as_str());
    span.record("provider", provider_name.as_str());
    span.record("streaming", stream);

    let norm_req = build_normalized_request(&body, canonical_model.clone());

    let request_id = format!("chatcmpl-mr-{}", uuid::Uuid::new_v4());
    let start = Instant::now();

    if stream {
        let adapter = state
            .provider_registry
            .get(&provider_name)
            .map_err(ApiError::ProviderError)?;
        let sse_stream = adapter
            .stream(&norm_req)
            .await
            .map_err(ApiError::ProviderError)?;

        let messages_json = serde_json::to_string(
            &body["messages"].as_array().cloned().unwrap_or_default(),
        )
        .unwrap_or_default();

        let logged_stream = log_streaming_request(
            sse_stream,
            StreamLogCtx {
                state: state.clone(),
                user_id: user.id,
                user_name: user.name.clone(),
                model: model.clone(),
                canonical_model: canonical_model.clone(),
                provider: provider_name.clone(),
                messages_json,
                start,
            },
        );

        return Ok(
            streaming_response(Box::pin(logged_stream), request_id).into_response(),
        );
    }

    let mut current_model = canonical_model.clone();
    let mut current_provider = provider_name.clone();
    let result = loop {
        let adapter = state
            .provider_registry
            .get(&current_provider)
            .map_err(ApiError::ProviderError)?;
        match adapter
            .complete(&build_normalized_request(&body, current_model.clone()))
            .instrument(tracing::info_span!(
                "modelrouter.provider_call",
                "provider.name" = current_provider.as_str()
            ))
            .await
        {
            Ok(r) => break r,
            Err(e) => {
                tracing::warn!(
                    model = current_model.as_str(),
                    provider = current_provider.as_str(),
                    error = %e,
                    "Provider call failed, checking fallback chain"
                );
                if let Some(next_model) = state.fallback.next_after(&current_model) {
                    let (next_provider, next_canonical) = state.router.resolve(next_model);
                    current_model = next_canonical;
                    current_provider = next_provider;
                    tracing::info!(fallback_model = current_model.as_str(), "Retrying with fallback");
                } else {
                    return Err(ApiError::ProviderError(e));
                }
            }
        }
    };
    let latency_ms = start.elapsed().as_millis() as i64;
    let cost = state
        .cost_calc
        .calculate(&canonical_model, result.prompt_tokens, result.completion_tokens);

    span.record("cost.usd", cost);
    span.record("tokens.prompt", result.prompt_tokens as u64);

    #[cfg(feature = "otel")]
    {
        crate::telemetry::metrics::record_request(&canonical_model, &provider_name, "ok");
        crate::telemetry::metrics::record_tokens(
            &canonical_model, &provider_name,
            result.prompt_tokens, result.completion_tokens,
        );
        crate::telemetry::metrics::record_cost(
            &canonical_model, &provider_name, user.id, cost,
        );
        crate::telemetry::metrics::record_duration(
            &canonical_model, &provider_name, false, latency_ms as f64,
        );
    }

    #[cfg(feature = "prometheus")]
    if let Some(ref metrics) = state.app_metrics {
        metrics.record_request(&current_model, &current_provider, "ok");
        metrics.record_tokens(&current_model, &current_provider, result.prompt_tokens, result.completion_tokens);
        metrics.record_cost(&current_model, &current_provider, cost);
    }

    // Fire-and-forget: log prompt + cost
    let state_clone = state.clone();
    let model_clone = model.clone();
    let canonical_clone = canonical_model.clone();
    let provider_clone = provider_name.clone();
    let messages_json = serde_json::to_string(
        &body["messages"].as_array().cloned().unwrap_or_default(),
    )
    .unwrap_or_default();
    let response_clone = result.content.clone();
    let finish_clone = result.finish_reason.clone();
    let user_id = user.id;
    let user_name_clone = user.name.clone();
    let prompt_tokens = result.prompt_tokens;
    let completion_tokens = result.completion_tokens;

    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model: model_clone.clone(),
            routed_model: canonical_clone.clone(),
            provider: provider_clone.clone(),
            messages: messages_json,
            response: Some(response_clone),
            finish_reason: Some(finish_clone),
            prompt_tokens: prompt_tokens as i64,
            completion_tokens: completion_tokens as i64,
            cost_usd: cost,
            latency_ms: Some(latency_ms),
            tags: "[]".to_string(),
            project: None,
        };
        match PromptRepository::create(&*state_clone.db, prompt).await {
            Ok(saved_prompt) => {
                let ledger = NewCostLedgerEntry {
                    user_id,
                    prompt_id: saved_prompt.id,
                    model: canonical_clone.clone(),
                    provider: provider_clone,
                    project: None,
                    tokens_in: prompt_tokens as i64,
                    tokens_out: completion_tokens as i64,
                    cost_usd: cost,
                };
                if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
                    tracing::error!("Failed to record cost: {}", e);
                }
            }
            Err(e) => tracing::error!("Failed to record prompt: {}", e),
        }

        // Fire on_response_sent lifecycle hooks
        for hook in &state_clone.settings.hooks.lifecycle {
            if hook.event == "on_response_sent" {
                let payload = crate::hooks::lifecycle::response_sent_payload(
                    &user_name_clone,
                    &model_clone,
                    &canonical_clone,
                    cost,
                    latency_ms,
                );
                crate::hooks::lifecycle::fire(hook, payload);
            }
        }
    });

    Ok(Json(build_openai_response(request_id, &result)).into_response())
}

struct StreamLogCtx {
    state: AppState,
    user_id: i64,
    user_name: String,
    model: String,
    canonical_model: String,
    provider: String,
    messages_json: String,
    start: Instant,
}

/// Wraps an SSE stream so that, when the terminal `[DONE]` chunk passes through,
/// a tokio task is spawned to record the prompt and cost in the DB.
fn log_streaming_request(
    stream: crate::providers::adapter::SseStream,
    ctx: StreamLogCtx,
) -> impl futures::Stream<Item = anyhow::Result<bytes::Bytes>> + Send {
    use futures::StreamExt;

    let accumulated = Arc::new(Mutex::new(String::new()));
    let accumulated_clone = accumulated.clone();

    let cost_calc = ctx.state.cost_calc.clone();
    let db = ctx.state.db.clone();
    let lifecycle_hooks = ctx.state.settings.hooks.lifecycle.clone();
    let user_id = ctx.user_id;
    let user_name = ctx.user_name;
    let model = ctx.model;
    let canonical_model = ctx.canonical_model;
    let provider = ctx.provider;
    let messages_json = ctx.messages_json;
    let start = ctx.start;

    stream.map(move |chunk_result| {
        if let Ok(ref chunk) = chunk_result {
            if let Some(text) = extract_text_from_sse(chunk) {
                if let Ok(mut acc) = accumulated_clone.lock() {
                    acc.push_str(&text);
                }
            }

            // Detect end of stream
            let is_done = std::str::from_utf8(chunk)
                .map(|s| s.contains("[DONE]"))
                .unwrap_or(false);

            if is_done {
                let content = accumulated_clone
                    .lock()
                    .map(|a| a.clone())
                    .unwrap_or_default();
                let completion_tokens = (content.chars().count() / 4) as u32;
                let prompt_tokens = (messages_json.chars().count() / 4) as u32;
                let cost = cost_calc.calculate(&canonical_model, prompt_tokens, completion_tokens);
                let latency_ms = start.elapsed().as_millis() as i64;

                let db_c = db.clone();
                let model_c = model.clone();
                let canonical_c = canonical_model.clone();
                let provider_c = provider.clone();
                let messages_c = messages_json.clone();
                let user_name_c = user_name.clone();
                let lifecycle_hooks_c = lifecycle_hooks.clone();

                tokio::spawn(async move {
                    use crate::db::repositories::{
                        costs::CostRepository, prompts::PromptRepository,
                    };

                    let model_c_ref = model_c.clone();
                    let prompt = NewPrompt {
                        user_id,
                        session_id: None,
                        request_model: model_c,
                        routed_model: canonical_c.clone(),
                        provider: provider_c.clone(),
                        messages: messages_c,
                        response: Some(content),
                        finish_reason: Some("stop".to_string()),
                        prompt_tokens: prompt_tokens as i64,
                        completion_tokens: completion_tokens as i64,
                        cost_usd: cost,
                        latency_ms: Some(latency_ms),
                        tags: "[]".to_string(),
                        project: None,
                    };
                    match PromptRepository::create(&*db_c, prompt).await {
                        Ok(saved) => {
                            let entry = NewCostLedgerEntry {
                                user_id,
                                prompt_id: saved.id,
                                model: canonical_c.clone(),
                                provider: provider_c,
                                project: None,
                                tokens_in: prompt_tokens as i64,
                                tokens_out: completion_tokens as i64,
                                cost_usd: cost,
                            };
                            if let Err(e) = CostRepository::create(&*db_c, entry).await {
                                tracing::error!("Failed to log streaming cost: {}", e);
                            }
                        }
                        Err(e) => tracing::error!("Failed to log streaming prompt: {}", e),
                    }

                    // Fire on_response_sent lifecycle hooks
                    for hook in &lifecycle_hooks_c {
                        if hook.event == "on_response_sent" {
                            let payload = crate::hooks::lifecycle::response_sent_payload(
                                &user_name_c,
                                &model_c_ref,
                                &canonical_c,
                                cost,
                                latency_ms,
                            );
                            crate::hooks::lifecycle::fire(hook, payload);
                        }
                    }
                });
            }
        }
        chunk_result
    })
}

fn build_normalized_request(
    body: &Value,
    model: String,
) -> crate::providers::adapter::NormalizedRequest {
    crate::providers::adapter::NormalizedRequest {
        model,
        messages: body["messages"].as_array().cloned().unwrap_or_default(),
        stream: body["stream"].as_bool().unwrap_or(false),
        temperature: body["temperature"].as_f64(),
        max_tokens: body["max_tokens"].as_u64().map(|v| v as u32),
        extra_params: serde_json::Value::Object(Default::default()),
    }
}

fn build_openai_response(
    request_id: String,
    result: &crate::providers::adapter::CompletionResult,
) -> Value {
    serde_json::json!({
        "id": request_id,
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": result.content
            },
            "finish_reason": result.finish_reason
        }],
        "usage": {
            "prompt_tokens": result.prompt_tokens,
            "completion_tokens": result.completion_tokens,
            "total_tokens": result.prompt_tokens + result.completion_tokens
        }
    })
}

fn streaming_response(
    sse_stream: crate::providers::adapter::SseStream,
    _request_id: String,
) -> impl IntoResponse {
    use axum::body::Body;
    use axum::http::{header, StatusCode};
    use axum::response::Response;
    use futures::TryStreamExt;

    let body = Body::from_stream(
        sse_stream.map_err(|e| std::io::Error::other(e.to_string())),
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(body)
        .unwrap()
}

/// Extract text content from an SSE chunk for token estimation.
/// Returns Some(text) for data chunks, None for [DONE] or invalid.
pub fn extract_text_from_sse(chunk: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(chunk).ok()?;
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if data.trim() == "[DONE]" {
                return None;
            }
            if let Ok(json) = serde_json::from_str::<Value>(data) {
                let content = json["choices"][0]["delta"]["content"].as_str()?;
                return Some(content.to_string());
            }
        }
    }
    None
}
