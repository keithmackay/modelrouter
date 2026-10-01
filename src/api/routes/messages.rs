use axum::{extract::State, response::{IntoResponse, Response}, Json};
use serde_json::Value;
use tracing::Instrument;

use axum::http::HeaderValue;

use crate::{
    api::routes::completions::{record_cache_hit, CacheHitCtx, CACHE_HEADER},
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::models::{NewCostLedgerEntry, NewPrompt},
    router::cache::stream::{message_as_completion, messages_replay_sse},
    router::policy::PolicyDecision,
};

async fn log_messages_cost(
    state: &AppState,
    user_id: i64,
    api_key_id: Option<i64>,
    project: Option<String>,
    user_name: &str,
    model: &str,
    canonical_model: &str,
    provider: &str,
    messages_json: &str,
    prompt_tokens: u32,
    completion_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
    cost: f64,
    latency_ms: i64,
    attribution: crate::api::attribution::Attribution,
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
        cache_read_tokens: cache_read_tokens as i64,
        cache_write_tokens: cache_write_tokens as i64,
        cost_usd: cost,
        latency_ms: Some(latency_ms),
        ttft_ms: None,
        attempts: None,
        tags: "[]".to_string(),
        project: project.clone(),
        attribution_correlation_id: attribution.correlation_id.clone(),
        attribution_tags: attribution.tags_json(),
        experiment_id: None,
        experiment_variant: None,
    };
    // Storage policy (issue #4): the prompt row is optional; the cost row is not.
    let stored = match crate::db::prompt_store::apply_storage_policy(&state.storage.load(), prompt) {
        Some(p) => match PromptRepository::create(&*state.prompt_db, p).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::error!("Failed to record prompt: {}", e);
                None
            }
        },
        None => None,
    };
    let ledger = NewCostLedgerEntry {
        user_id,
        prompt_id: stored.as_ref().map(|s| s.id),
        model: canonical_model.to_string(),
        provider: provider.to_string(),
        project: project.clone(),
        tokens_in: prompt_tokens as i64,
        tokens_out: completion_tokens as i64,
        cost_usd: cost,
        api_key_id,
        attribution_correlation_id: attribution.correlation_id.clone(),
        attribution_tags: attribution.tags_json(),
        experiment_id: None,
        experiment_variant: None,
        tokens_estimated: false,
    };
    if let Err(e) = CostRepository::create(&*state.db, ledger).await {
        tracing::error!("Failed to record cost: {}", e);
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

/// The body forwarded upstream: the caller's native Anthropic request with the
/// canonical model name, and any OpenAI `image_url` parts mixed into its
/// content translated to native `image` blocks (issue #86) — Anthropic rejects
/// the OpenAI tag. Shared by the streaming and non-streaming calls.
fn build_upstream_body(body: &Value, canonical_model: &str) -> Value {
    let mut upstream = body.clone();
    upstream["model"] = Value::String(canonical_model.to_string());
    if let Some(messages) = upstream["messages"].as_array_mut() {
        crate::providers::anthropic::translate_native_messages(messages);
    }
    upstream
}

pub async fn anthropic_messages(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!(
        "anthropic_messages",
        user_id = tracing::field::Empty,
        model = tracing::field::Empty,
        streaming = tracing::field::Empty,
    );
    anthropic_messages_inner(State(state), user, headers, Json(body))
        .instrument(span)
        .await
}

async fn anthropic_messages_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use std::time::Instant;

    crate::api::routes::reject_experiment_header("/v1/messages", &headers)?;
    let cache_directives = crate::router::cache::CacheDirectives::from_headers(&headers)
        .map_err(ApiError::InvalidRequest)?;
    let user = user.0;
    let attribution = crate::api::attribution::Attribution::extract(&body, &headers)?;
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

    // Check load balancer: if `model` is a named pool, override provider + model.
    // Operator-disabled entries are skipped when selecting (issue #5).
    let lb_choice = state
        .load_balancer
        .resolve_available(&model, |p, m| state.router.is_available(p, m));
    let (resolved_provider, canonical_model) = if let Some((lb_provider, lb_model)) = lb_choice {
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
    } else if state.load_balancer.is_pool(&model) {
        return Err(ApiError::Disabled(format!(
            "every model in load balancer pool '{model}' has been disabled by an administrator"
        )));
    } else {
        state.router.resolve(&model)
    };

    // Operator disable gate (issue #5) — 403 naming the reason, never a provider call.
    state.router.check_available(&resolved_provider, &canonical_model)?;

    let span = tracing::Span::current();
    span.record("user_id", user.id);
    span.record("model", model.as_str());
    span.record("streaming", stream);

    // ── Response cache ───────────────────────────────────────────────────────
    // The chat-completions eligibility rules, over the native request body. A
    // streamed and a plain call share one entry: the stored payload is the
    // `message` object either way.
    let cache_plan = if state.policy.cache_enabled(&user, &canonical_model) {
        state
            .response_cache
            .completion_plan(cache_directives.mode, &body)
    } else {
        crate::router::cache::CachePlan::Skip
    };
    let cache_key = cache_plan
        .store()
        .then(|| crate::router::cache::messages_cache_key(&canonical_model, &body));
    if let (true, Some(key)) = (cache_plan.lookup(), cache_key.as_ref()) {
        if let Some(message) = state
            .response_cache
            .get_message(key, &canonical_model)
            .await
        {
            return Ok(serve_cached_message(
                &state,
                key,
                message,
                stream,
                &user,
                &attribution,
                &model,
                &canonical_model,
                &body,
            ));
        }
    }

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
    // `model` (not `requested_model`) — reflects the alias actually being
    // dispatched under, in case an experiment/complexity downgrade remapped
    // it to a different tier. See TierTimeoutsConfig::resolve.
    let timeout_secs = state.settings.tier_timeouts.resolve(&model, anthropic_config.timeout_secs);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| ApiError::ProviderError(e.into()))?;

    let upstream_url = format!("{}/v1/messages", api_base);
    let start = Instant::now();

    let upstream_body = build_upstream_body(&body, &canonical_model);

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
            state.circuit_breaker.record_upstream_status("anthropic", status);
            return Err(ApiError::ProviderError(anyhow::anyhow!(
                "Anthropic API error {}: {}",
                status,
                err_text
            )));
        }
        state.circuit_breaker.record_success("anthropic");

        use axum::body::Body;
        use axum::http::{header, StatusCode};
        use futures::StreamExt;

        let mut capture = cache_key.clone().map(|key| {
            (
                key,
                crate::router::cache::stream::MessagesStreamCapture::new(),
            )
        });
        let cache_state = state.clone();
        let cache_model = canonical_model.clone();
        let stream_directives = cache_directives.clone();
        let byte_stream = upstream_resp.bytes_stream().map(move |chunk| {
            match &chunk {
                Ok(bytes) => {
                    if let Some((_, cap)) = capture.as_mut() {
                        cap.feed(bytes);
                        if cap.is_done() {
                            if let Some((key, cap)) = capture.take() {
                                store_streamed_message(
                                    &cache_state,
                                    key,
                                    &cache_model,
                                    cap,
                                    stream_directives.clone(),
                                );
                            }
                        }
                    }
                }
                // A broken stream is never stored.
                Err(_) => capture = None,
            }
            chunk.map_err(|e| std::io::Error::other(e.to_string()))
        });

        // Fix 3: Fire-and-forget approximate cost for streaming
        let state_c = state.clone();
        let user_id = user.id;
        let api_key_id_s = user.api_key_id;
        let user_project_s = attribution.project_or(user.api_key_project.clone());
        let user_name_s = user.name.clone();
        let model_s = model.clone();
        let canonical_s = canonical_model.clone();
        let provider_s = "anthropic".to_string();
        let messages_json_s = serde_json::to_string(
            &body["messages"].as_array().cloned().unwrap_or_default()
        ).unwrap_or_default();
        let start_s = start;
        let attribution_s = attribution.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await; // let stream initiate
            let prompt_tokens = (messages_json_s.chars().count() / 4) as u32;
            let cost = state_c.cost_calc.calculate(&canonical_s, prompt_tokens, 0);
            let latency_ms = start_s.elapsed().as_millis() as i64;
            log_messages_cost(&state_c, user_id, api_key_id_s, user_project_s, &user_name_s, &model_s, &canonical_s, &provider_s,
                               &messages_json_s, prompt_tokens, 0, 0, 0, cost, latency_ms,
                               attribution_s).await;
        });

        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("X-Accel-Buffering", "no")
            .body(Body::from_stream(byte_stream))
            .unwrap();
        if let Some(outcome) = cache_plan.miss_header() {
            response
                .headers_mut()
                .insert(CACHE_HEADER, HeaderValue::from_static(outcome));
        }

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
        state.circuit_breaker.record_upstream_status("anthropic", status);
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
    let cache_read_tokens = resp_json["usage"]["cache_read_input_tokens"]
        .as_u64()
        .unwrap_or(0) as u32;
    let cache_write_tokens = resp_json["usage"]["cache_creation_input_tokens"]
        .as_u64()
        .unwrap_or(0) as u32;
    let stop_reason = resp_json["stop_reason"]
        .as_str()
        .unwrap_or("end_turn")
        .to_string();

    let cost = state.cost_calc.calculate_with_cache(
        &canonical_model,
        prompt_tokens,
        completion_tokens,
        cache_read_tokens,
        cache_write_tokens,
    );

    if let Some(ref key) = cache_key {
        state
            .response_cache
            .put_message(
                key,
                &canonical_model,
                resp_json.clone(),
                cost,
                &cache_directives,
            )
            .await;
    }

    // Fix 1 & Fix 4: Capture user_name before spawn; use model_clone consistently (no model_c)
    let state_clone = state.clone();
    let model_clone = model.clone();
    let canonical_c = canonical_model.clone();
    let user_name_c = user.name.clone();
    let api_key_id_c = user.api_key_id;
    let project = user.api_key_project.clone();
    let messages_json = serde_json::to_string(
        &body["messages"].as_array().cloned().unwrap_or_default(),
    )
    .unwrap_or_default();
    let response_content = serde_json::to_string(&resp_json).unwrap_or_default();
    let user_id = user.id;
    let attr_correlation = attribution.correlation_id.clone();
    let attr_tags = attribution.tags_json();

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
            cache_read_tokens: cache_read_tokens as i64,
            cache_write_tokens: cache_write_tokens as i64,
            cost_usd: cost,
            latency_ms: Some(latency_ms),
            ttft_ms: None,
            attempts: None,
            tags: "[]".to_string(),
            project: project.clone(),
            attribution_correlation_id: attr_correlation.clone(),
            attribution_tags: attr_tags.clone(),
            experiment_id: None,
            experiment_variant: None,
        };
        // Storage policy (issue #4): the prompt row is optional; the cost row is not.
        let stored = match crate::db::prompt_store::apply_storage_policy(&state_clone.storage.load(), prompt) {
            Some(p) => match PromptRepository::create(&*state_clone.prompt_db, p).await {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::error!("Failed to record prompt: {}", e);
                    None
                }
            },
            None => None,
        };
        {
                let ledger = NewCostLedgerEntry {
                    user_id,
                    prompt_id: stored.as_ref().map(|s| s.id),
                    model: canonical_c.clone(),
                    provider: "anthropic".to_string(),
                    project: project.clone(),
                    tokens_in: prompt_tokens as i64,
                    tokens_out: completion_tokens as i64,
                    cost_usd: cost,
                    api_key_id: api_key_id_c,
                    attribution_correlation_id: attr_correlation.clone(),
                    attribution_tags: attr_tags.clone(),
                    experiment_id: None,
                    experiment_variant: None,
                    tokens_estimated: false,
                };
                if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
                    tracing::error!("Failed to record cost: {}", e);
                }
                let mut event = crate::callbacks::CallbackEvent {
                    trace_id: stored.as_ref().map(|s| s.id.to_string()).unwrap_or_else(|| "0".to_string()),
                    user_id,
                    model: canonical_c.clone(),
                    provider: "anthropic".to_string(),
                    input: serde_json::from_str(&messages_json).unwrap_or(serde_json::Value::Null),
                    output: response_content.clone(),
                    prompt_tokens,
                    completion_tokens,
                    cost_usd: cost,
                    latency_ms,
                };
                // The row above was redacted; the egress must be too (issue #53).
                crate::db::prompt_store::redact_callback_content(&state_clone.storage.load(), &mut event);
                state_clone.callbacks.dispatch(event);
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

    let mut response = Json(resp_json).into_response();
    if let Some(outcome) = cache_plan.miss_header() {
        response
            .headers_mut()
            .insert(CACHE_HEADER, HeaderValue::from_static(outcome));
    }
    Ok(response)
}

/// Store a stream's assembled `message`, priced from its own usage, once the
/// capture confirms the stream ended cleanly.
fn store_streamed_message(
    state: &AppState,
    key: String,
    canonical_model: &str,
    capture: crate::router::cache::stream::MessagesStreamCapture,
    directives: crate::router::cache::CacheDirectives,
) {
    let Some(message) = capture.finish() else {
        return;
    };
    let usage = message_as_completion(&message);
    let cost = state.cost_calc.calculate_with_cache(
        canonical_model,
        usage.prompt_tokens,
        usage.completion_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
    );
    let state = state.clone();
    let model = canonical_model.to_string();
    tokio::spawn(async move {
        state
            .response_cache
            .put_message(&key, &model, message, cost, &directives)
            .await;
    });
}

/// Answer from a cached `message`: the JSON as-is, or replayed as the Messages
/// event stream when the request asked to stream. The hit is metered like a
/// chat-completions hit (zero spend, the avoided cost as savings).
#[allow(clippy::too_many_arguments)]
fn serve_cached_message(
    state: &AppState,
    key: &str,
    mut message: Value,
    stream: bool,
    user: &crate::db::models::User,
    attribution: &crate::api::attribution::Attribution,
    request_model: &str,
    canonical_model: &str,
    body: &Value,
) -> Response {
    tracing::info!(
        cache_key = key,
        model = canonical_model,
        streamed = stream,
        "response cache hit"
    );
    let usage = message_as_completion(&message);
    let avoided_cost = state.cost_calc.calculate_with_cache(
        canonical_model,
        usage.prompt_tokens,
        usage.completion_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
    );
    record_cache_hit(
        state,
        CacheHitCtx {
            user_id: user.id,
            api_key_id: user.api_key_id,
            user_project: attribution.project_or(user.api_key_project.clone()),
            request_model: request_model.to_string(),
            canonical_model: canonical_model.to_string(),
            provider: "anthropic".to_string(),
            messages_json: serde_json::to_string(
                &body["messages"].as_array().cloned().unwrap_or_default(),
            )
            .unwrap_or_default(),
            avoided_cost,
            skip_log: false,
            attribution: attribution.clone(),
        },
        &usage,
    );
    // Each response gets its own id, as a live call would.
    message["id"] = Value::String(format!("msg_mr_{}", uuid::Uuid::new_v4().simple()));
    let mut response = if stream {
        Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
            .header(axum::http::header::CACHE_CONTROL, "no-cache")
            .body(axum::body::Body::from(messages_replay_sse(&message)))
            .unwrap()
    } else {
        Json(message).into_response()
    };
    response
        .headers_mut()
        .insert(CACHE_HEADER, HeaderValue::from_static("HIT"));
    response
}
