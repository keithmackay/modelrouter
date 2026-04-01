use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::{extract::State, response::IntoResponse, Json};
use serde_json::Value;

use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::{
        models::{NewCostLedgerEntry, NewPrompt},
    },
};

pub async fn chat_completions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;
    let model = body["model"]
        .as_str()
        .unwrap_or(&state.settings.routing.default_model)
        .to_string();
    let stream = body["stream"].as_bool().unwrap_or(false);

    let (provider_name, canonical_model) = state.router.resolve(&model);

    let norm_req = build_normalized_request(&body, canonical_model.clone());

    let adapter = state
        .provider_registry
        .get(&provider_name)
        .map_err(ApiError::ProviderError)?;

    let request_id = format!("chatcmpl-mr-{}", uuid::Uuid::new_v4());
    let start = Instant::now();

    if stream {
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

    let result = adapter
        .complete(&norm_req)
        .await
        .map_err(ApiError::ProviderError)?;
    let latency_ms = start.elapsed().as_millis() as i64;
    let cost = state
        .cost_calc
        .calculate(&canonical_model, result.prompt_tokens, result.completion_tokens);

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
    let prompt_tokens = result.prompt_tokens;
    let completion_tokens = result.completion_tokens;

    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model: model_clone,
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
                    model: canonical_clone,
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
    });

    Ok(Json(build_openai_response(request_id, &result)).into_response())
}

struct StreamLogCtx {
    state: AppState,
    user_id: i64,
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
    let user_id = ctx.user_id;
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

                tokio::spawn(async move {
                    use crate::db::repositories::{
                        costs::CostRepository, prompts::PromptRepository,
                    };

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
                                model: canonical_c,
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
