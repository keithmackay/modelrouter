use axum::{extract::State, response::{IntoResponse, Response}, Json};
use serde_json::Value;
use tracing::Instrument;

use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::models::{NewCostLedgerEntry, NewPrompt},
    router::policy::PolicyDecision,
};

async fn log_messages_cost(
    state: &AppState,
    user_id: i64,
    api_key_id: Option<i64>,
    user_name: &str,
    model: &str,
    canonical_model: &str,
    provider: &str,
    messages_json: &str,
    prompt_tokens: u32,
    completion_tokens: u32,
    cost: f64,
    latency_ms: i64,
) {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let prompt = NewPrompt {
        user_id,
        session_id: None,
        request_model: model.to_string(),
        routed_model: canonical_model.to_string(),
        provider: provider.to_string(),
        messages: messages_json.to_string(),
        response: None,
        finish_reason: None,
        prompt_tokens: prompt_tokens as i64,
        completion_tokens: completion_tokens as i64,
        cost_usd: cost,
        latency_ms: Some(latency_ms),
        tags: "[]".to_string(),
        project: None,
    };
    match PromptRepository::create(&*state.db, prompt).await {
        Ok(saved_prompt) => {
            let ledger = NewCostLedgerEntry {
                user_id,
                prompt_id: saved_prompt.id,
                model: canonical_model.to_string(),
                provider: provider.to_string(),
                project: None,
                tokens_in: prompt_tokens as i64,
                tokens_out: completion_tokens as i64,
                cost_usd: cost,
                api_key_id,
            };
            if let Err(e) = CostRepository::create(&*state.db, ledger).await {
                tracing::error!("Failed to record cost: {}", e);
            }
        }
        Err(e) => tracing::error!("Failed to record prompt: {}", e),
    }

    // Fire on_response_sent lifecycle hooks
    for hook in &state.settings.hooks.lifecycle {
        if hook.event == "on_response_sent" {
            let payload = crate::hooks::lifecycle::response_sent_payload(
                user_name,
                model,
                canonical_model,
                cost,
                latency_ms,
            );
            crate::hooks::lifecycle::fire(hook, payload);
        }
    }
}

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
    use std::time::Instant;

    let user = user.0;
    let requested_model = body["model"]
        .as_str()
        .unwrap_or(&state.settings.routing.default_model)
        .to_string();
    let messages_for_complexity = body["messages"].as_array().cloned().unwrap_or_default();
    let model = state.complexity_router.maybe_downgrade(&requested_model, &messages_for_complexity);
    let stream = body["stream"].as_bool().unwrap_or(false);

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
                    None => return Err(ApiError::PolicyDenied {
                        reason: "concurrent request limit exceeded".to_string(),
                        status: 429,
                    }),
                }
            } else {
                None
            }
        }
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
    };

    // Check load balancer: if `model` is a named pool, override provider + model
    let (_lb_provider_unused, canonical_model) = if let Some((lb_provider, lb_model)) =
        state.load_balancer.resolve(&model)
    {
        if lb_provider != "anthropic" {
            tracing::warn!(
                pool = model.as_str(),
                lb_provider = lb_provider.as_str(),
                "load balancer pool entry has non-anthropic provider; /v1/messages only supports Anthropic — provider field is ignored"
            );
        }
        tracing::info!(
            pool = model.as_str(),
            routed_model = lb_model.as_str(),
            "load balancer selected model for /v1/messages"
        );
        (lb_provider, lb_model)  // only lb_model is used by this handler
    } else {
        state.router.resolve(&model)
    };

    let span = tracing::Span::current();
    span.record("user_id", user.id);
    span.record("model", model.as_str());
    span.record("streaming", stream);

    // Fix 2: Always use the "anthropic" provider config for the Messages API
    let anthropic_config = state.settings.providers.get("anthropic")
        .ok_or_else(|| ApiError::ProviderError(anyhow::anyhow!("No 'anthropic' provider configured")))?
        .clone();

    let api_base = anthropic_config
        .api_base
        .as_deref()
        .unwrap_or("https://api.anthropic.com")
        .trim_end_matches('/')
        .to_string();
    let api_key = anthropic_config.api_key.clone();
    let timeout_secs = anthropic_config.timeout_secs;

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
        if state.circuit_breaker.is_open("anthropic") {
            tracing::warn!(provider = "anthropic", "circuit breaker open, skipping provider");
            return Err(ApiError::ProviderError(anyhow::anyhow!("circuit breaker open for anthropic")));
        }
        // Streaming: proxy raw SSE bytes back to client
        let upstream_resp = client
            .post(&upstream_url)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&upstream_body)
            .send()
            .await
            .map_err(|e| {
                state.circuit_breaker.record_failure("anthropic");
                ApiError::ProviderError(e.into())
            })?;

        if !upstream_resp.status().is_success() {
            let status = upstream_resp.status().as_u16();
            let err_text = upstream_resp
                .text()
                .await
                .unwrap_or_else(|_| "upstream error".to_string());
            state.circuit_breaker.record_failure("anthropic");
            return Err(ApiError::ProviderError(anyhow::anyhow!(
                "Anthropic API error {}: {}",
                status,
                err_text
            )));
        }
        state.circuit_breaker.record_success("anthropic");

        use axum::body::Body;
        use axum::http::{header, StatusCode};
        use futures::TryStreamExt;

        let byte_stream = upstream_resp
            .bytes_stream()
            .map_err(|e| std::io::Error::other(e.to_string()));

        // Fix 3: Fire-and-forget approximate cost for streaming
        let state_c = state.clone();
        let user_id = user.id;
        let api_key_id_s = user.api_key_id;
        let user_name_s = user.name.clone();
        let model_s = model.clone();
        let canonical_s = canonical_model.clone();
        let provider_s = "anthropic".to_string();
        let messages_json_s = serde_json::to_string(
            &body["messages"].as_array().cloned().unwrap_or_default()
        ).unwrap_or_default();
        let start_s = start;
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await; // let stream initiate
            let prompt_tokens = (messages_json_s.chars().count() / 4) as u32;
            let cost = state_c.cost_calc.calculate(&canonical_s, prompt_tokens, 0);
            let latency_ms = start_s.elapsed().as_millis() as i64;
            log_messages_cost(&state_c, user_id, api_key_id_s, &user_name_s, &model_s, &canonical_s, &provider_s,
                               &messages_json_s, prompt_tokens, 0, cost, latency_ms).await;
        });

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
    if state.circuit_breaker.is_open("anthropic") {
        tracing::warn!(provider = "anthropic", "circuit breaker open, skipping provider");
        return Err(ApiError::ProviderError(anyhow::anyhow!("circuit breaker open for anthropic")));
    }
    let upstream_resp = client
        .post(&upstream_url)
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&upstream_body)
        .send()
        .await
        .map_err(|e| {
            state.circuit_breaker.record_failure("anthropic");
            ApiError::ProviderError(e.into())
        })?;

    let latency_ms = start.elapsed().as_millis() as i64;

    if !upstream_resp.status().is_success() {
        let status = upstream_resp.status().as_u16();
        let err_text = upstream_resp
            .text()
            .await
            .unwrap_or_else(|_| "upstream error".to_string());
        state.circuit_breaker.record_failure("anthropic");
        return Err(ApiError::ProviderError(anyhow::anyhow!(
            "Anthropic API error {}: {}",
            status,
            err_text
        )));
    }
    state.circuit_breaker.record_success("anthropic");

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

    // Fix 1 & Fix 4: Capture user_name before spawn; use model_clone consistently (no model_c)
    let state_clone = state.clone();
    let model_clone = model.clone();
    let canonical_c = canonical_model.clone();
    let user_name_c = user.name.clone();
    let api_key_id_c = user.api_key_id;
    let messages_json = serde_json::to_string(
        &body["messages"].as_array().cloned().unwrap_or_default(),
    )
    .unwrap_or_default();
    let response_content = serde_json::to_string(&resp_json).unwrap_or_default();
    let user_id = user.id;

    tokio::spawn(async move {
        use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model: model_clone.clone(),
            routed_model: canonical_c.clone(),
            provider: "anthropic".to_string(),
            messages: messages_json.clone(),
            response: Some(response_content.clone()),
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
                    provider: "anthropic".to_string(),
                    project: None,
                    tokens_in: prompt_tokens as i64,
                    tokens_out: completion_tokens as i64,
                    cost_usd: cost,
                    api_key_id: api_key_id_c,
                };
                if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
                    tracing::error!("Failed to record cost: {}", e);
                }
                state_clone.callbacks.dispatch(crate::callbacks::CallbackEvent {
                    trace_id: format!("{}", saved_prompt.id),
                    user_id,
                    model: canonical_c.clone(),
                    provider: "anthropic".to_string(),
                    input: serde_json::from_str(&messages_json).unwrap_or(serde_json::Value::Null),
                    output: response_content.clone(),
                    prompt_tokens,
                    completion_tokens,
                    cost_usd: cost,
                    latency_ms,
                });
            }
            Err(e) => tracing::error!("Failed to record prompt: {}", e),
        }

        // Fix 1: Fire on_response_sent lifecycle hooks with correct user_name
        for hook in &state_clone.settings.hooks.lifecycle {
            if hook.event == "on_response_sent" {
                let payload = crate::hooks::lifecycle::response_sent_payload(
                    &user_name_c,
                    &model_clone,
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
