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
    headers: axum::http::HeaderMap,
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
    // Capture context BEFORE the handler consumes these, so a failure can still
    // be attributed after the fact. See api::failure_log for why capture wraps
    // the whole handler rather than sitting at each `return Err(...)`.
    let ctx = crate::api::failure_log::context_from_request(
        "/v1/chat/completions",
        &state,
        &user.0,
        &headers,
        &body,
    );
    let started = Instant::now();

    let result = chat_completions_inner(State(state.clone()), user, headers, Json(body))
        .instrument(span)
        .await;

    if let Err(err) = &result {
        let ctx = crate::api::failure_log::FailureContext {
            latency_ms: Some(started.elapsed().as_millis() as i64),
            ..ctx
        };
        crate::api::failure_log::record_failure(&state, ctx, err).await;
    }
    result
}

async fn chat_completions_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    // Config-level gate (issue #4) rides the same rail as x-no-log: both mean
    // "record cost, skip the prompt row".
    let skip_log = should_skip_logging(&headers) || !state.storage.load().store_prompts;
    let user = user.0;
    tracing::Span::current().record("user_id", user.id);
    // Read attribution from the request as it arrived: pipeline hooks may
    // rewrite the body, and attribution describes the caller's intent, not the
    // rewritten request.
    let attribution = crate::api::attribution::Attribution::extract(&body, &headers)?;
    let requested_model = body["model"]
        .as_str()
        .unwrap_or(&state.settings.routing.default_model)
        .to_string();
    let messages_for_complexity = body["messages"].as_array().cloned().unwrap_or_default();
    let model = state.complexity_router.maybe_downgrade(&requested_model, &messages_for_complexity);
    let stream = body["stream"].as_bool().unwrap_or(false);

    // The response-cache lookup happens further down, after the policy check and
    // model resolution: a cache hit must still be an authorized request, and the
    // key must be built from the *resolved* model.

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
    };

    // Session rate limit check
    if let Some(session_id) = body["session_id"].as_str() {
        let estimated_tokens = body["messages"]
            .as_array()
            .map(|m| m.iter().map(|msg| {
                msg["content"].as_str().map(|s| (s.len() / 4) as u32).unwrap_or(50)
            }).sum::<u32>())
            .unwrap_or(100);
        if !state.session_limiter.check_and_record(session_id, estimated_tokens) {
            return Err(ApiError::PolicyDenied {
                reason: "session rate limit exceeded".to_string(),
                status: 429,
            });
        }
    }

    // Pre-request guardrail check
    let guardrail_ctx = crate::guardrails::GuardrailContext {
        messages: body["messages"].clone(),
        model: model.clone(),
        user_id: user.id,
    };
    match state.guardrails.check_request(&guardrail_ctx).await {
        crate::guardrails::GuardrailDecision::Allow => {}
        crate::guardrails::GuardrailDecision::Block { reason } => {
            return Err(ApiError::PolicyDenied { reason, status: 400 });
        }
        crate::guardrails::GuardrailDecision::Replace { .. } => {
            // Replace on request is not supported; treat as Allow
        }
    }

    // Validate that `tools` and `tool_choice` are not present (issue #41)
    if let Some(tools) = body["tools"].as_array() {
        if !tools.is_empty() {
            return Err(ApiError::InvalidRequest(
                "`tools` is not supported by this endpoint/model; grounded search belongs on /v1/search".to_string()
            ));
        }
    }
    if body.get("tool_choice").is_some() {
        return Err(ApiError::InvalidRequest(
            "`tool_choice` is not supported by this endpoint/model; grounded search belongs on /v1/search".to_string()
        ));
    }

    // Inject per-key metadata for pipeline hooks before running them
    let mut body = body;
    let session_window = user.session_window_secs.unwrap_or(28800);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("_mr_session_window_secs".to_string(), serde_json::json!(session_window));
    }
    let body = body;

    // Run pre_request pipeline hooks (may mutate body)
    let body = crate::hooks::pipeline::run_pre_request(
        &state.settings.hooks.pipeline,
        &state.db,
        body,
    )
    .await
    .map_err(|_| ApiError::Internal)?;

    // Check load balancer: if `model` is a named pool, override provider + model.
    // Operator-disabled entries are skipped when selecting (issue #5).
    let lb_choice = state
        .load_balancer
        .resolve_available(&model, |p, m| state.router.is_available(p, m));
    let (provider_name, canonical_model) = if let Some((lb_provider, lb_model)) = lb_choice {
        tracing::info!(
            pool = model.as_str(),
            provider = lb_provider.as_str(),
            routed_model = lb_model.as_str(),
            "load balancer selected provider"
        );
        (lb_provider, lb_model)
    } else if state.load_balancer.is_pool(&model) {
        // A pool exists but every member is disabled — say so rather than
        // silently falling through to the default model.
        return Err(ApiError::Disabled(format!(
            "every model in load balancer pool '{model}' has been disabled by an administrator"
        )));
    } else {
        crate::api::routes::guard_model_substitution(&state, &model)?;
        state.router.resolve(&model)
    };

    // Session stickiness — pin this session to the resolved provider
    let (provider_name, canonical_model) = if let Some(session_id) = body["session_id"].as_str() {
        use crate::router::session_affinity::resolve_with_pin;
        let skip_affinity = should_skip_affinity(&headers);
        let pin = if skip_affinity {
            None
        } else {
            state.session_affinity.get(session_id)
        };
        let (pinned_provider, pinned_model, should_update) =
            resolve_with_pin(pin.as_ref(), &provider_name, &canonical_model);
        if should_update {
            state.session_affinity.set(session_id, &pinned_provider, &pinned_model);
        }
        (pinned_provider, pinned_model)
    } else {
        (provider_name, canonical_model)
    };

    // Operator disable gate (issue #5). Checked before the cache, the circuit
    // breaker and any provider dispatch, so a disabled model or provider is
    // never called and the caller gets a 403 naming the reason rather than a
    // provider error or a silent reroute.
    state
        .router
        .check_available(&provider_name, &canonical_model)?;

    let span = tracing::Span::current();
    span.record("model", canonical_model.as_str());
    span.record("provider", provider_name.as_str());
    span.record("streaming", stream);

    // ── Response cache ───────────────────────────────────────────────────────
    // Eligibility is conservative (see `router::cache`): streaming and
    // nondeterministic sampling are never served from cache.
    let cache_key = if state.policy.cache_enabled(&user, &canonical_model)
        && state.response_cache.completion_eligible(&body)
    {
        Some(crate::router::cache::completion_cache_key(&canonical_model, &body))
    } else {
        None
    };

    if let Some(ref key) = cache_key {
        if let Some(cached) = state
            .response_cache
            .get_completion(key, &canonical_model)
            .await
        {
            tracing::info!(
                cache_key = key.as_str(),
                model = canonical_model.as_str(),
                "response cache hit"
            );
            // What the call would have cost. Recorded as a saving, not as spend.
            let avoided_cost = state.cost_calc.calculate_with_cache(
                &canonical_model,
                cached.prompt_tokens,
                cached.completion_tokens,
                cached.cache_read_tokens,
                cached.cache_write_tokens,
            );
            record_cache_hit(
                &state,
                CacheHitCtx {
                    user_id: user.id,
                    api_key_id: user.api_key_id,
                    user_project: attribution.project_or(user.api_key_project.clone()),
                    request_model: model.clone(),
                    canonical_model: canonical_model.clone(),
                    provider: provider_name.clone(),
                    messages_json: serde_json::to_string(
                        &body["messages"].as_array().cloned().unwrap_or_default(),
                    )
                    .unwrap_or_default(),
                    avoided_cost,
                    skip_log,
                    attribution: attribution.clone(),
                },
                &cached,
            );
            let request_id = format!("chatcmpl-mr-{}", uuid::Uuid::new_v4());
            let mut response =
                Json(build_openai_response(request_id, &canonical_model, &cached)).into_response();
            response
                .headers_mut()
                .insert(CACHE_HEADER, axum::http::HeaderValue::from_static("HIT"));
            return Ok(response);
        }
    }

    let norm_req =
        build_normalized_request(&body, canonical_model.clone(), &state.settings.model_capabilities);

    let request_id = format!("chatcmpl-mr-{}", uuid::Uuid::new_v4());
    let start = Instant::now();

    if stream {
        if state.circuit_breaker.is_open(&provider_name) {
            tracing::warn!(provider = provider_name.as_str(), "circuit breaker open, skipping provider");
            let pseudo_err = anyhow::anyhow!("circuit breaker open for {}", provider_name);
            return Err(ApiError::ProviderError(pseudo_err));
        }
        let adapter = state
            .provider_registry
            .get(&provider_name)
            .map_err(ApiError::ProviderError)?;
        let sse_stream = adapter
            .stream(&norm_req)
            .await
            .map_err(|e| {
                state
                    .circuit_breaker
                    .record_provider_error(&provider_name, &e.to_string());
                ApiError::ProviderError(e)
            })?;
        state.circuit_breaker.record_success(&provider_name);

        let messages_json = serde_json::to_string(
            &body["messages"].as_array().cloned().unwrap_or_default(),
        )
        .unwrap_or_default();

        let logged_stream = log_streaming_request(
            sse_stream,
            StreamLogCtx {
                state: state.clone(),
                user_id: user.id,
                api_key_id: user.api_key_id,
                user_project: attribution.project_or(user.api_key_project.clone()),
                user_name: user.name.clone(),
                model: model.clone(),
                canonical_model: canonical_model.clone(),
                provider: provider_name.clone(),
                messages_json,
                start,
                skip_log,
                attribution: attribution.clone(),
            },
        );

        return Ok(
            streaming_response(Box::pin(logged_stream), request_id).into_response(),
        );
    }

    let retry_policy = crate::router::retry::RetryPolicy::from_config(&state.settings.retry);
    let mut current_model = canonical_model.clone();
    let mut current_provider = provider_name.clone();
    let result = loop {
        if state.circuit_breaker.is_open(&current_provider) {
            tracing::warn!(provider = current_provider.as_str(), "circuit breaker open, skipping provider");
            let pseudo_err = anyhow::anyhow!("circuit breaker open for {}", current_provider);
            if let Some((next_provider, next_canonical)) =
                next_available_fallback(&state, &current_model)
            {
                current_model = next_canonical;
                current_provider = next_provider;
                continue;
            } else {
                return Err(ApiError::ProviderError(pseudo_err));
            }
        }
        let adapter = state
            .provider_registry
            .get(&current_provider)
            .map_err(ApiError::ProviderError)?;
        let mut retry_attempt = 0u32;
        let call_result = loop {
            match adapter
                .complete(&build_normalized_request(
                    &body,
                    current_model.clone(),
                    &state.settings.model_capabilities,
                ))
                .instrument(tracing::info_span!(
                    "modelrouter.provider_call",
                    "provider.name" = current_provider.as_str()
                ))
                .await
            {
                Ok(r) => break Ok(r),
                Err(e) => {
                    let err_str = e.to_string();
                    let retryable = crate::router::retry::RetryableError::classify(&err_str);
                    if retry_policy.should_retry(retry_attempt, &retryable) {
                        let delay = retry_policy.delay_ms(retry_attempt);
                        tracing::warn!(
                            attempt = retry_attempt,
                            delay_ms = delay,
                            provider = current_provider.as_str(),
                            error = %err_str,
                            "provider error, retrying with backoff"
                        );
                        tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                        retry_attempt += 1;
                        continue;
                    }
                    break Err(e);
                }
            }
        };
        match call_result {
            Ok(r) => {
                state.circuit_breaker.record_success(&current_provider);
                break r;
            }
            Err(e) => {
                state
                    .circuit_breaker
                    .record_provider_error(&current_provider, &e.to_string());
                tracing::warn!(
                    model = current_model.as_str(),
                    provider = current_provider.as_str(),
                    error = %e,
                    "Provider call failed, checking fallback chain"
                );
                if let Some((next_provider, next_canonical)) =
                    next_available_fallback(&state, &current_model)
                {
                    current_model = next_canonical;
                    current_provider = next_provider;
                    tracing::info!(fallback_model = current_model.as_str(), "Retrying with fallback");
                } else {
                    return Err(ApiError::ProviderError(e));
                }
            }
        }
    };

    // Post-response guardrail check (non-streaming only)
    let result = match state.guardrails.check_response(&guardrail_ctx, &result.content).await {
        crate::guardrails::GuardrailDecision::Allow => result,
        crate::guardrails::GuardrailDecision::Block { reason } => {
            return Err(ApiError::PolicyDenied { reason, status: 400 });
        }
        crate::guardrails::GuardrailDecision::Replace { content } => {
            let mut r = result;
            r.content = content;
            r
        }
    };

    let latency_ms = start.elapsed().as_millis() as i64;
    let cost = state.cost_calc.calculate_with_cache(
        &canonical_model,
        result.prompt_tokens,
        result.completion_tokens,
        result.cache_read_tokens,
        result.cache_write_tokens,
    );

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
    let api_key_id = user.api_key_id;
    let user_project = attribution.project_or(user.api_key_project.clone());
    let user_name_clone = user.name.clone();
    let prompt_tokens = result.prompt_tokens;
    let completion_tokens = result.completion_tokens;
    let cache_read_tokens = result.cache_read_tokens;
    let cache_write_tokens = result.cache_write_tokens;

    let skip_log_clone = skip_log;
    let attr_correlation = attribution.correlation_id.clone();
    let attr_tags = attribution.tags_json();
    tokio::spawn(async move {
        if !skip_log_clone {
            let prompt = NewPrompt {
                user_id,
                session_id: None,
                request_model: model_clone.clone(),
                routed_model: canonical_clone.clone(),
                provider: provider_clone.clone(),
                messages: messages_json.clone(),
                response: Some(response_clone.clone()),
                finish_reason: Some(finish_clone),
                prompt_tokens: prompt_tokens as i64,
                completion_tokens: completion_tokens as i64,
                cache_read_tokens: cache_read_tokens as i64,
                cache_write_tokens: cache_write_tokens as i64,
                cost_usd: cost,
                latency_ms: Some(latency_ms),
                tags: "[]".to_string(),
                project: user_project.clone(),
                attribution_correlation_id: attr_correlation.clone(),
                attribution_tags: attr_tags.clone(),
            };
            let mut prompt = prompt;
            crate::db::prompt_store::redact_prompt_content(&state_clone.storage.load(), &mut prompt);
            match PromptRepository::create(&*state_clone.prompt_db, prompt).await {
                Ok(saved_prompt) => {
                    let ledger = NewCostLedgerEntry {
                        user_id,
                        prompt_id: Some(saved_prompt.id),
                        model: canonical_clone.clone(),
                        provider: provider_clone.clone(),
                        project: user_project.clone(),
                        tokens_in: prompt_tokens as i64,
                        tokens_out: completion_tokens as i64,
                        cost_usd: cost,
                        api_key_id,
                        attribution_correlation_id: attr_correlation.clone(),
                        attribution_tags: attr_tags.clone(),
                    };
                    if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
                        tracing::error!("Failed to record cost: {}", e);
                    }
                    let mut event = crate::callbacks::CallbackEvent {
                        trace_id: format!("{}", saved_prompt.id),
                        user_id,
                        model: canonical_clone.clone(),
                        provider: provider_clone.clone(),
                        input: serde_json::from_str(&messages_json).unwrap_or(serde_json::Value::Null),
                        output: response_clone.clone(),
                        prompt_tokens,
                        completion_tokens,
                        cost_usd: cost,
                        latency_ms,
                    };
                    // The row above was redacted; the egress must be too (issue #53).
                    crate::db::prompt_store::redact_callback_content(&state_clone.storage.load(), &mut event);
                    state_clone.callbacks.dispatch(event);
                }
                Err(e) => tracing::error!("Failed to record prompt: {}", e),
            }
        } else {
            // Skip logging but still record cost for budget enforcement
            let ledger = NewCostLedgerEntry {
                user_id,
                prompt_id: None,
                model: canonical_clone.clone(),
                provider: provider_clone.clone(),
                project: user_project.clone(),
                tokens_in: prompt_tokens as i64,
                tokens_out: completion_tokens as i64,
                cost_usd: cost,
                api_key_id,
                attribution_correlation_id: attr_correlation.clone(),
                attribution_tags: attr_tags.clone(),
            };
            if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
                tracing::error!("Failed to record cost: {}", e);
            }
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

    // Store result in cache for future requests. `cost` rides along so a later
    // hit can report what it saved.
    if let Some(key) = cache_key {
        state
            .response_cache
            .put_completion(&key, &canonical_model, &result, cost)
            .await;
    }

    let mut response = Json(build_openai_response(request_id, &canonical_model, &result)).into_response();
    response
        .headers_mut()
        .insert(CACHE_HEADER, axum::http::HeaderValue::from_static("MISS"));
    Ok(response)
}

/// Response header telling callers whether the body came from the router cache.
pub const CACHE_HEADER: &str = "x-modelrouter-cache";

/// Everything needed to meter a cache hit, gathered at the call site so the
/// spawned task borrows nothing.
struct CacheHitCtx {
    user_id: i64,
    api_key_id: Option<i64>,
    user_project: Option<String>,
    request_model: String,
    canonical_model: String,
    provider: String,
    messages_json: String,
    avoided_cost: f64,
    skip_log: bool,
    attribution: crate::api::attribution::Attribution,
}

/// Record a cache hit as usage: a prompt row (unless logging is skipped) and a
/// cost-ledger row with `cache_hit = true`, `cost_usd = 0`, and the avoided cost
/// in `saved_usd`. Fire-and-forget, matching the live-call logging path.
/// Next fallback candidate after `current_model` that an operator has not disabled.
///
/// Operator-disabled entries are *skipped*, not fatal: the chain exists to find a
/// working alternative, and a disable means "do not use this one". Bounded by the
/// chain length so a chain that loops back on itself terminates.
fn next_available_fallback(
    state: &AppState,
    current_model: &str,
) -> Option<(String, String)> {
    const MAX_FALLBACK_HOPS: usize = 16;

    let mut cursor = current_model.to_string();
    for _ in 0..MAX_FALLBACK_HOPS {
        let next_model = state.fallback.next_after(&cursor)?;
        let (next_provider, next_canonical) = state.router.resolve(&next_model);
        if state.router.is_available(&next_provider, &next_canonical) {
            return Some((next_provider, next_canonical));
        }
        tracing::info!(
            skipped_model = next_model.as_str(),
            "fallback candidate is disabled by an administrator, trying the next one"
        );
        cursor = next_model;
    }
    None
}

fn record_cache_hit(
    state: &AppState,
    ctx: CacheHitCtx,
    result: &crate::providers::adapter::CompletionResult,
) {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    #[cfg(feature = "otel")]
    {
        crate::telemetry::metrics::record_request(&ctx.canonical_model, &ctx.provider, "cache_hit");
    }
    #[cfg(feature = "prometheus")]
    if let Some(ref metrics) = state.app_metrics {
        metrics.record_request(&ctx.canonical_model, &ctx.provider, "cache_hit");
    }

    let state = state.clone();
    let result = result.clone();
    tokio::spawn(async move {
        // `cost_usd` here is the *avoided* cost; `create_cache_hit` writes it to
        // saved_usd and forces cost_usd to zero, so it can never inflate spend.
        let ledger = NewCostLedgerEntry {
            user_id: ctx.user_id,
            prompt_id: None,
            model: ctx.canonical_model.clone(),
            provider: ctx.provider.clone(),
            project: ctx.user_project.clone(),
            tokens_in: result.prompt_tokens as i64,
            tokens_out: result.completion_tokens as i64,
            cost_usd: ctx.avoided_cost,
            api_key_id: ctx.api_key_id,
            attribution_correlation_id: ctx.attribution.correlation_id.clone(),
            attribution_tags: ctx.attribution.tags_json(),
        };

        let prompt_id = if ctx.skip_log {
            None
        } else {
            let prompt = NewPrompt {
                user_id: ctx.user_id,
                session_id: None,
                request_model: ctx.request_model.clone(),
                routed_model: ctx.canonical_model.clone(),
                provider: ctx.provider.clone(),
                messages: ctx.messages_json.clone(),
                response: Some(result.content.clone()),
                finish_reason: Some(result.finish_reason.clone()),
                prompt_tokens: result.prompt_tokens as i64,
                completion_tokens: result.completion_tokens as i64,
                cache_read_tokens: result.cache_read_tokens as i64,
                cache_write_tokens: result.cache_write_tokens as i64,
                // Zero: the router paid nothing for this response.
                cost_usd: 0.0,
                latency_ms: Some(0),
                tags: "[]".to_string(),
                project: ctx.user_project.clone(),
                attribution_correlation_id: ctx.attribution.correlation_id.clone(),
                attribution_tags: ctx.attribution.tags_json(),
            };
            let mut prompt = prompt;
            crate::db::prompt_store::redact_prompt_content(&state.storage.load(), &mut prompt);
            match PromptRepository::create(&*state.prompt_db, prompt).await {
                Ok(saved) => Some(saved.id),
                Err(e) => {
                    tracing::error!("Failed to record cache-hit prompt: {}", e);
                    None
                }
            }
        };

        let ledger = NewCostLedgerEntry { prompt_id, ..ledger };
        if let Err(e) = CostRepository::create_cache_hit(&*state.db, ledger).await {
            tracing::error!("Failed to record cache-hit usage: {}", e);
        }
    });
}

struct StreamLogCtx {
    state: AppState,
    user_id: i64,
    api_key_id: Option<i64>,
    user_project: Option<String>,
    user_name: String,
    model: String,
    canonical_model: String,
    provider: String,
    messages_json: String,
    start: Instant,
    skip_log: bool,
    attribution: crate::api::attribution::Attribution,
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
    let prompt_db = ctx.state.prompt_db.clone();
    let storage = ctx.state.storage.clone();
    let lifecycle_hooks = ctx.state.settings.hooks.lifecycle.clone();
    let user_id = ctx.user_id;
    let api_key_id = ctx.api_key_id;
    let user_project = ctx.user_project;
    let user_name = ctx.user_name;
    let model = ctx.model;
    let canonical_model = ctx.canonical_model;
    let provider = ctx.provider;
    let messages_json = ctx.messages_json;
    let start = ctx.start;
    let skip_log = ctx.skip_log;
    let attr_correlation = ctx.attribution.correlation_id.clone();
    let attr_tags = ctx.attribution.tags_json();

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
                let prompt_db_c = prompt_db.clone();
                let storage_c = storage.clone();
                let model_c = model.clone();
                let canonical_c = canonical_model.clone();
                let provider_c = provider.clone();
                let messages_c = messages_json.clone();
                let user_name_c = user_name.clone();
                let lifecycle_hooks_c = lifecycle_hooks.clone();
                let user_project_c = user_project.clone();
                let attr_correlation_c = attr_correlation.clone();
                let attr_tags_c = attr_tags.clone();

                tokio::spawn(async move {
                    use crate::db::repositories::{
                        costs::CostRepository, prompts::PromptRepository,
                    };

                    let model_c_ref = model_c.clone();
                    if !skip_log {
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
                            cache_read_tokens: 0,
                            cache_write_tokens: 0,
                            cost_usd: cost,
                            latency_ms: Some(latency_ms),
                            tags: "[]".to_string(),
                            project: user_project_c.clone(),
                            attribution_correlation_id: attr_correlation_c.clone(),
                            attribution_tags: attr_tags_c.clone(),
                        };
                        let mut prompt = prompt;
                        crate::db::prompt_store::redact_prompt_content(&storage_c.load(), &mut prompt);
                        match PromptRepository::create(&*prompt_db_c, prompt).await {
                            Ok(saved) => {
                                let entry = NewCostLedgerEntry {
                                    user_id,
                                    prompt_id: Some(saved.id),
                                    model: canonical_c.clone(),
                                    provider: provider_c,
                                    project: user_project_c.clone(),
                                    tokens_in: prompt_tokens as i64,
                                    tokens_out: completion_tokens as i64,
                                    cost_usd: cost,
                                    api_key_id,
                                    attribution_correlation_id: attr_correlation_c.clone(),
                                    attribution_tags: attr_tags_c.clone(),
                                };
                                if let Err(e) = CostRepository::create(&*db_c, entry).await {
                                    tracing::error!("Failed to log streaming cost: {}", e);
                                }
                            }
                            Err(e) => tracing::error!("Failed to log streaming prompt: {}", e),
                        }
                    } else {
                        // Skip logging but still record cost for budget enforcement
                        let entry = NewCostLedgerEntry {
                            user_id,
                            prompt_id: None,
                            model: canonical_c.clone(),
                            provider: provider_c,
                            project: user_project_c.clone(),
                            tokens_in: prompt_tokens as i64,
                            tokens_out: completion_tokens as i64,
                            cost_usd: cost,
                            api_key_id,
                            attribution_correlation_id: attr_correlation_c.clone(),
                            attribution_tags: attr_tags_c.clone(),
                        };
                        if let Err(e) = CostRepository::create(&*db_c, entry).await {
                            tracing::error!("Failed to log streaming cost: {}", e);
                        }
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
    capabilities: &[crate::config::schema::ModelCapabilityEntry],
) -> crate::providers::adapter::NormalizedRequest {
    // Drop sampling parameters the resolved model rejects. Callers address a
    // routing alias and cannot know what it resolves to, so forwarding
    // `temperature` verbatim to a Claude 5 model turns every such request into
    // a 400 — and, before the breaker learned to ignore client errors, took the
    // whole provider down with it. Stripping here rather than in each adapter
    // covers every provider from one place.
    let temperature = body["temperature"].as_f64().filter(|_| {
        let supported =
            crate::router::model_capabilities::supports_temperature(&model, capabilities);
        if !supported {
            tracing::debug!(
                model = model.as_str(),
                "model does not accept `temperature`; dropping it from the provider request"
            );
        }
        supported
    });

    crate::providers::adapter::NormalizedRequest {
        model,
        messages: body["messages"].as_array().cloned().unwrap_or_default(),
        stream: body["stream"].as_bool().unwrap_or(false),
        temperature,
        max_tokens: body["max_tokens"].as_u64().map(|v| v as u32),
        extra_params: serde_json::Value::Object(Default::default()),
    }
}

fn build_openai_response(
    request_id: String,
    model: &str,
    result: &crate::providers::adapter::CompletionResult,
) -> Value {
    serde_json::json!({
        "id": request_id,
        "object": "chat.completion",
        // The concrete backing model this request actually dispatched to —
        // not the caller's requested alias/pool name. Per the OpenAI
        // chat-completions contract, `model` should report what served the
        // request; omitting it left OpenAI-compatible clients unable to
        // learn the resolved model at all, and the `ai` SDK fell back to
        // the requested id, corrupting the caller's cost attribution.
        "model": model,
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

pub fn should_skip_affinity(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-session-lb")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub fn should_skip_logging(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-no-log")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
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

#[cfg(test)]
mod openai_response_tests {
    use super::build_openai_response;
    use crate::providers::adapter::CompletionResult;

    #[test]
    fn includes_the_resolved_backing_model() {
        // Issue: the response previously omitted "model" entirely, so an
        // OpenAI-compatible client (e.g. the `ai` npm SDK) could not learn
        // which concrete model actually served the request and silently
        // fell back to echoing the client's own requested alias instead.
        let result = CompletionResult {
            content: "hello".to_string(),
            prompt_tokens: 1,
            completion_tokens: 1,
            finish_reason: "stop".to_string(),
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        let response = build_openai_response(
            "chatcmpl-mr-test".to_string(),
            "gpt-4o-2026-01-01",
            &result,
        );
        assert_eq!(response["model"], "gpt-4o-2026-01-01");
    }
}

#[cfg(test)]
mod affinity_tests {
    use super::should_skip_affinity;
    use axum::http::HeaderMap;

    #[test]
    fn detects_x_session_lb_true() {
        let mut h = HeaderMap::new();
        h.insert("x-session-lb", "true".parse().unwrap());
        assert!(should_skip_affinity(&h));
    }

    #[test]
    fn x_session_lb_false_is_not_skipped() {
        let mut h = HeaderMap::new();
        h.insert("x-session-lb", "false".parse().unwrap());
        assert!(!should_skip_affinity(&h));
    }

    #[test]
    fn absent_x_session_lb_is_false() {
        assert!(!should_skip_affinity(&HeaderMap::new()));
    }

    #[test]
    fn case_insensitive_true() {
        let mut h = HeaderMap::new();
        h.insert("x-session-lb", "TRUE".parse().unwrap());
        assert!(should_skip_affinity(&h));
    }
}

#[cfg(test)]
mod no_log_tests {
    use super::should_skip_logging;
    use axum::http::HeaderMap;

    #[test]
    fn detects_true_value() {
        let mut h = HeaderMap::new();
        h.insert("x-no-log", "true".parse().unwrap());
        assert!(should_skip_logging(&h));
    }
    #[test]
    fn ignores_false_value() {
        let mut h = HeaderMap::new();
        h.insert("x-no-log", "false".parse().unwrap());
        assert!(!should_skip_logging(&h));
    }
    #[test]
    fn absent_header_is_false() {
        assert!(!should_skip_logging(&HeaderMap::new()));
    }
    #[test]
    fn case_insensitive() {
        let mut h = HeaderMap::new();
        h.insert("x-no-log", "TRUE".parse().unwrap());
        assert!(should_skip_logging(&h));
    }
}
