use axum::{extract::State, response::{IntoResponse, Response}, Json};
use serde_json::Value;
use tracing::Instrument;

use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::models::{NewCostLedgerEntry, NewPrompt},
    router::policy::PolicyDecision,
};

pub async fn anthropic_messages(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!(
        "anthropic_messages",
        user_id = tracing::field::Empty,
        model = tracing::field::Empty,
        streaming = tracing::field::Empty,
    );
    anthropic_messages_inner(State(state), user, Json(body))
        .instrument(span)
        .await
}

async fn anthropic_messages_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};
    use std::time::Instant;

    let user = user.0;
    let model = body["model"]
        .as_str()
        .unwrap_or(&state.settings.routing.default_model)
        .to_string();
    let stream = body["stream"].as_bool().unwrap_or(false);

    let (provider_name, canonical_model) = state.router.resolve(&model);

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
            return Err(ApiError::PolicyDenied { reason, status });
        }
    }

    let span = tracing::Span::current();
    span.record("user_id", user.id);
    span.record("model", model.as_str());
    span.record("streaming", stream);

    // Get the provider config using resolved provider name
    let provider_config = state
        .settings
        .providers
        .get(&provider_name)
        .ok_or_else(|| ApiError::InvalidRequest(format!("No '{}' provider configured", provider_name)))?
        .clone();

    let api_base = provider_config
        .api_base
        .as_deref()
        .unwrap_or("https://api.anthropic.com")
        .trim_end_matches('/')
        .to_string();
    let api_key = provider_config.api_key.clone();
    let timeout_secs = provider_config.timeout_secs;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| ApiError::ProviderError(e.into()))?;

    let upstream_url = format!("{}/v1/messages", api_base);
    let start = Instant::now();

    // Build upstream body with canonical model name
    let mut upstream_body = body.clone();
    upstream_body["model"] = serde_json::Value::String(canonical_model.clone());

    if stream {
        // Streaming: proxy raw SSE bytes back to client
        let upstream_resp = client
            .post(&upstream_url)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&upstream_body)
            .send()
            .await
            .map_err(|e| ApiError::ProviderError(e.into()))?;

        if !upstream_resp.status().is_success() {
            let status = upstream_resp.status().as_u16();
            let err_text = upstream_resp
                .text()
                .await
                .unwrap_or_else(|_| "upstream error".to_string());
            return Err(ApiError::ProviderError(anyhow::anyhow!(
                "Anthropic API error {}: {}",
                status,
                err_text
            )));
        }

        use axum::body::Body;
        use axum::http::{header, StatusCode};
        use futures::TryStreamExt;

        let byte_stream = upstream_resp
            .bytes_stream()
            .map_err(|e| std::io::Error::other(e.to_string()));

        let response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("X-Accel-Buffering", "no")
            .body(Body::from_stream(byte_stream))
            .unwrap();

        return Ok(response);
    }

    // Non-streaming: proxy and return raw Anthropic JSON
    let upstream_resp = client
        .post(&upstream_url)
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&upstream_body)
        .send()
        .await
        .map_err(|e| ApiError::ProviderError(e.into()))?;

    let latency_ms = start.elapsed().as_millis() as i64;

    if !upstream_resp.status().is_success() {
        let status = upstream_resp.status().as_u16();
        let err_text = upstream_resp
            .text()
            .await
            .unwrap_or_else(|_| "upstream error".to_string());
        return Err(ApiError::ProviderError(anyhow::anyhow!(
            "Anthropic API error {}: {}",
            status,
            err_text
        )));
    }

    let resp_json: Value = upstream_resp
        .json()
        .await
        .map_err(|e| ApiError::ProviderError(e.into()))?;

    // Extract usage from Anthropic response for cost logging
    let prompt_tokens = resp_json["usage"]["input_tokens"]
        .as_u64()
        .unwrap_or(0) as u32;
    let completion_tokens = resp_json["usage"]["output_tokens"]
        .as_u64()
        .unwrap_or(0) as u32;
    let stop_reason = resp_json["stop_reason"]
        .as_str()
        .unwrap_or("end_turn")
        .to_string();

    let cost = state
        .cost_calc
        .calculate(&canonical_model, prompt_tokens, completion_tokens);

    // Fire-and-forget: log prompt + cost
    let state_clone = state.clone();
    let model_clone = model.clone();
    let canonical_c = canonical_model.clone();
    let provider_clone = provider_name.clone();
    let model_c = model.clone();
    let messages_json = serde_json::to_string(
        &body["messages"].as_array().cloned().unwrap_or_default(),
    )
    .unwrap_or_default();
    let response_content = serde_json::to_string(&resp_json).unwrap_or_default();
    let user_id = user.id;

    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model: model_clone.clone(),
            routed_model: canonical_c.clone(),
            provider: provider_clone.clone(),
            messages: messages_json,
            response: Some(response_content),
            finish_reason: Some(stop_reason),
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
                    model: canonical_c.clone(),
                    provider: provider_clone.clone(),
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
                    &provider_clone,
                    &model_c,
                    &canonical_c,
                    cost,
                    latency_ms,
                );
                crate::hooks::lifecycle::fire(hook, payload);
            }
        }
    });

    Ok(Json(resp_json).into_response())
}
