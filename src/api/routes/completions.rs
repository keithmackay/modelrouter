use std::time::Instant;

use axum::{extract::State, response::{IntoResponse, Response}, Json};
use serde_json::Value;
use tracing::Instrument;

use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    config::schema::StorageConfig,
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
    let x_no_log = should_skip_logging(&headers);
    let user = user.0;
    tracing::Span::current().record("user_id", user.id);
    // Read attribution from the request as it arrived: pipeline hooks may
    // rewrite the body, and attribution describes the caller's intent, not the
    // rewritten request.
    let attribution = crate::api::attribution::Attribution::extract(&body, &headers)?;
    // Experiment binding (spec §7a). `None` when the header is absent, in
    // which case nothing below changes. A bound request is pinned to its
    // variant's `provider/model`, so the adaptive layers — complexity
    // downgrade, load balancer, session affinity, response cache, fallback —
    // all stand aside: an experiment measures the pinned model, not the
    // router's opinion of it.
    let binding = state
        .experiments
        .bind(
            &headers,
            &body,
            attribution.correlation_id.as_deref(),
            user.id,
            chrono::Utc::now().timestamp(),
        )
        .map_err(|e| super::experiment_bind_rejected("/v1/chat/completions", e))?;
    let experiment_id = binding.as_ref().map(|b| b.experiment_id);
    let experiment_variant = binding.as_ref().map(|b| b.variant.clone());
    let retain_content = binding.as_ref().is_some_and(|b| b.retain_content);

    // Config-level gate (issue #4) rides the same rail as x-no-log: both mean
    // "record cost, skip the prompt row". A retaining experiment overrides the
    // config gate — its prompt rows are the experiment's evidence — but never
    // x-no-log, which is the caller's own request. Callback egress stays on
    // `skip_log`: an experiment retaining content locally does not widen what
    // leaves the router.
    let global_storage = state.storage.load();
    let skip_log = x_no_log || !global_storage.store_prompts;
    let write_prompt = !x_no_log && (global_storage.store_prompts || retain_content);
    // The storage policy the prompt row is redacted under: the operator's,
    // with content storage forced on for a retaining binding.
    let effective_storage = if retain_content {
        StorageConfig {
            store_prompts: true,
            store_prompt_content: true,
            ..(**global_storage).clone()
        }
    } else {
        (**global_storage).clone()
    };
    drop(global_storage);

    let requested_model = body["model"]
        .as_str()
        .unwrap_or(&state.settings.routing.default_model)
        .to_string();
    let messages_for_complexity = body["messages"].as_array().cloned().unwrap_or_default();
    let model = match &binding {
        // The overlay wins where it names the requested model; a name it does
        // not map routes as usual (and is still stamped). Neither is downgraded.
        Some(b) => b
            .overlay
            .get(&requested_model)
            .cloned()
            .unwrap_or_else(|| requested_model.clone()),
        None => state
            .complexity_router
            .maybe_downgrade(&requested_model, &messages_for_complexity),
    };
    if let Some(b) = &binding {
        tracing::info!(
            experiment_id = b.experiment_id,
            variant = b.variant.as_str(),
            requested_model = requested_model.as_str(),
            model = model.as_str(),
            "request bound to experiment variant"
        );
    }
    // What the rows record as the request model. Under a binding that is the
    // name the caller sent: the overlay is the experiment's doing, and the
    // comparison later runs on the caller's name.
    let logged_model = if binding.is_some() {
        requested_model.clone()
    } else {
        model.clone()
    };
    let stream = body["stream"].as_bool().unwrap_or(false);

    // The response-cache lookup happens further down, after the policy check and
    // model resolution: a cache hit must still be an authorized request, and the
    // key must be built from the *resolved* model.

    fire_request_received_hooks(&state, &user.name, &model, &body);

    // Policy check. The permit (when a concurrency cap applies) is held for the
    // rest of the request.
    let _concurrency_permit = enforce_policy(&state, &user, &model).await?;

    // `tool_choice` without `tools` is malformed on the OpenAI surface; reject
    // it here, BEFORE session limiter and guardrails, so a doomed request
    // doesn't consume budget or make external calls. Whether `tools` itself is
    // accepted depends on the resolved provider/model, so that check happens
    // after resolution (issue #88).
    validate_tool_choice_requires_tools(&body)?;

    check_session_rate_limit(&state, &body)?;

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

    // Load balancer pools, then session stickiness. A bound request never
    // consults the pool and neither reads nor writes an affinity pin: the
    // variant already fixed the model.
    let (provider_name, canonical_model) =
        resolve_provider_and_model(&state, binding.is_some(), &model)?;
    let session_for_affinity = body["session_id"].as_str().filter(|_| binding.is_none());
    let (provider_name, canonical_model) = apply_session_affinity(
        &state,
        &headers,
        session_for_affinity,
        provider_name,
        canonical_model,
    );

    // Operator disable gate (issue #5). Checked before the cache, the circuit
    // breaker and any provider dispatch, so a disabled model or provider is
    // never called and the caller gets a 403 naming the reason rather than a
    // provider error or a silent reroute.
    state
        .router
        .check_available(&provider_name, &canonical_model)?;

    // Tools capability gate (issue #88). `tools` used to be rejected outright
    // (issue #41 era), which broke every OpenAI-compat agentic client whose
    // backing model natively supports tool calling. Now the request is
    // rejected only when the RESOLVED adapter cannot forward tools — the only
    // point where that is knowable, since the caller addresses an alias.
    // Silently dropping tools instead would be worse than a 400: the model
    // would answer an agentic request in prose.
    let has_tools = request_has_tools(&body);
    if has_tools {
        let adapter = state
            .provider_registry
            .get(&provider_name)
            .map_err(ApiError::ProviderError)?;
        if !adapter.supports_tools(&canonical_model) {
            return Err(ApiError::InvalidRequest(format!(
                "`tools` is not supported for model '{canonical_model}' on provider '{provider_name}'"
            )));
        }
    }

    let span = tracing::Span::current();
    span.record("model", canonical_model.as_str());
    span.record("provider", provider_name.as_str());
    span.record("streaming", stream);

    // ── Response cache ───────────────────────────────────────────────────────
    // Eligibility is conservative (see `router::cache`): streaming and
    // nondeterministic sampling are never served from cache, and neither is
    // a bound request — a cached answer says nothing about the pinned model.
    let cache_key = if binding.is_none()
        && state.policy.cache_enabled(&user, &canonical_model)
        && state.response_cache.completion_eligible(&body)
    {
        Some(crate::router::cache::completion_cache_key(&canonical_model, &body))
    } else {
        None
    };

    if let Some(ref key) = cache_key {
        if let Some(response) = try_serve_cached_completion(
            &state,
            key,
            &model,
            &canonical_model,
            &provider_name,
            &user,
            &attribution,
            &body,
            skip_log,
        )
        .await
        {
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

        // The streaming path has no fallback loop, so the resolved pair is
        // also the pair that answers.
        let logged_stream = log_streaming_request(
            sse_stream,
            StreamLogCtx {
                state: state.clone(),
                user_id: user.id,
                api_key_id: user.api_key_id,
                user_project: attribution.project_or(user.api_key_project.clone()),
                user_name: user.name.clone(),
                model: logged_model.clone(),
                canonical_model: canonical_model.clone(),
                provider: provider_name.clone(),
                messages_json,
                start,
                write_prompt,
                storage: effective_storage,
                attribution: attribution.clone(),
                experiment_id,
                experiment_variant,
            },
        );

        return Ok(
            streaming_response(Box::pin(logged_stream), request_id).into_response(),
        );
    }

    let ProviderCallOutcome {
        result,
        provider: current_provider,
        model: current_model,
        attempts,
    } = complete_with_retry_and_fallback(
        &state,
        &user,
        &body,
        binding.is_some(),
        provider_name.clone(),
        canonical_model.clone(),
    )
    .await?;

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

    // From here on `current_model`/`current_provider` are the pair that
    // actually answered. After a fallback they differ from what was first
    // resolved, and pricing, the prompt and ledger rows and the response's
    // `model` must all name the answering model — a row priced at the primary's
    // rate for tokens the fallback produced misstates spend.
    let latency_ms = start.elapsed().as_millis() as i64;
    let cost = state.cost_calc.calculate_with_cache(
        &current_model,
        result.prompt_tokens,
        result.completion_tokens,
        result.cache_read_tokens,
        result.cache_write_tokens,
    );

    span.record("cost.usd", cost);
    span.record("tokens.prompt", result.prompt_tokens as u64);
    // Re-recorded so a fallback shows in the trace as well as the rows.
    span.record("model", current_model.as_str());
    span.record("provider", current_provider.as_str());

    record_success_metrics(&state, &current_model, &current_provider, user.id, &result, cost, latency_ms);

    // Fire-and-forget: log prompt + cost
    spawn_completion_logging(CompletionLogCtx {
        state: state.clone(),
        user_id: user.id,
        api_key_id: user.api_key_id,
        user_project: attribution.project_or(user.api_key_project.clone()),
        user_name: user.name.clone(),
        model: logged_model.clone(),
        canonical_model: current_model.clone(),
        provider: current_provider.clone(),
        messages_json: serde_json::to_string(
            &body["messages"].as_array().cloned().unwrap_or_default(),
        )
        .unwrap_or_default(),
        response: result.content.clone(),
        finish_reason: result.finish_reason.clone(),
        prompt_tokens: result.prompt_tokens,
        completion_tokens: result.completion_tokens,
        cache_read_tokens: result.cache_read_tokens,
        cache_write_tokens: result.cache_write_tokens,
        ttft_ms: result.ttft_ms,
        attempts: attempts.max(1),
        cost,
        latency_ms,
        write_prompt,
        skip_log,
        effective_storage,
        attribution_correlation_id: attribution.correlation_id.clone(),
        attribution_tags: attribution.tags_json(),
        experiment_id,
        experiment_variant: experiment_variant.clone(),
    });

    // Store result in cache for future requests. `cost` rides along so a later
    // hit can report what it saved.
    if let Some(key) = cache_key {
        state
            .response_cache
            .put_completion(&key, &canonical_model, &result, cost)
            .await;
    }

    let mut response = Json(build_openai_response(request_id, &current_model, &result)).into_response();
    response
        .headers_mut()
        .insert(CACHE_HEADER, axum::http::HeaderValue::from_static("MISS"));
    Ok(response)
}

/// Response header telling callers whether the body came from the router cache.
pub const CACHE_HEADER: &str = "x-modelrouter-cache";

/// Fire `on_request_received` lifecycle hooks.
fn fire_request_received_hooks(state: &AppState, user_name: &str, model: &str, body: &Value) {
    for hook in &state.settings.hooks.lifecycle {
        if hook.event == "on_request_received" {
            let payload = crate::hooks::lifecycle::request_received_payload(
                user_name,
                model,
                body["messages"].as_array().map(|m| m.len()).unwrap_or(0),
            );
            crate::hooks::lifecycle::fire(hook, payload);
        }
    }
}

/// Policy gate. Allow returns the concurrency permit to hold for the request's
/// lifetime (when a cap applies); Deny fires the budget hooks and denial
/// metrics and returns the error.
async fn enforce_policy(
    state: &AppState,
    user: &crate::db::models::User,
    model: &str,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, ApiError> {
    let policy_result = state
        .policy
        .check(user, model)
        .instrument(tracing::info_span!("modelrouter.policy_check"))
        .await
        .map_err(|_| ApiError::Internal)?;
    match policy_result {
        PolicyDecision::Allow { max_concurrent } => match max_concurrent {
            Some(max) => match state.concurrency.try_acquire(user.id, max) {
                Some(permit) => Ok(Some(permit)),
                None => Err(ApiError::PolicyDenied {
                    reason: "concurrent request limit exceeded".to_string(),
                    status: 429,
                }),
            },
            None => Ok(None),
        },
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
                            model,
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
                    model,
                    &state.router.resolve(model).0,
                    "policy_denied",
                );
                crate::telemetry::metrics::record_policy_denied(metric_reason);
            }
            Err(ApiError::PolicyDenied { reason, status })
        }
    }
}

/// Does the request carry a non-empty `tools` array? (issue #88)
fn request_has_tools(body: &Value) -> bool {
    body["tools"].as_array().is_some_and(|t| !t.is_empty())
}

/// A `tool_choice` steering the model toward tools is meaningless — and, on
/// the OpenAI surface, invalid — without a `tools` array to choose from.
/// Null and "none" are treated as absent (both mean: do not use tools).
fn validate_tool_choice_requires_tools(body: &Value) -> Result<(), ApiError> {
    if request_has_tools(body) {
        return Ok(());
    }
    if let Some(tc) = body.get("tool_choice") {
        if !tc.is_null() && tc.as_str() != Some("none") {
            return Err(ApiError::InvalidRequest(
                "`tool_choice` requires a non-empty `tools` array".to_string(),
            ));
        }
    }
    Ok(())
}

/// Session rate limit: estimated from message content length when present.
fn check_session_rate_limit(state: &AppState, body: &Value) -> Result<(), ApiError> {
    if let Some(session_id) = body["session_id"].as_str() {
        let estimated_tokens = body["messages"]
            .as_array()
            .map(|m| {
                m.iter()
                    .map(|msg| {
                        msg["content"].as_str().map(|s| (s.len() / 4) as u32).unwrap_or(50)
                    })
                    .sum::<u32>()
            })
            .unwrap_or(100);
        if !state.session_limiter.check_and_record(session_id, estimated_tokens) {
            return Err(ApiError::PolicyDenied {
                reason: "session rate limit exceeded".to_string(),
                status: 429,
            });
        }
    }
    Ok(())
}

/// Resolve `model` through the load balancer (named pools) or the router.
/// Operator-disabled pool entries are skipped when selecting (issue #5). A
/// bound request never consults the pool: a pool picks per request, and an
/// experiment must pin one concrete model.
fn resolve_provider_and_model(
    state: &AppState,
    bound: bool,
    model: &str,
) -> Result<(String, String), ApiError> {
    let lb_choice = if bound {
        None
    } else {
        state
            .load_balancer
            .resolve_available(model, |p, m| state.router.is_available(p, m))
    };
    if let Some((lb_provider, lb_model)) = lb_choice {
        tracing::info!(
            pool = model,
            provider = lb_provider.as_str(),
            routed_model = lb_model.as_str(),
            "load balancer selected provider"
        );
        Ok((lb_provider, lb_model))
    } else if bound && state.load_balancer.is_pool(model) {
        Err(ApiError::InvalidRequest(format!(
            "'{model}' is a load balancer pool; experiments must pin a concrete provider/model"
        )))
    } else if state.load_balancer.is_pool(model) {
        // A pool exists but every member is disabled — say so rather than
        // silently falling through to the default model.
        Err(ApiError::Disabled(format!(
            "every model in load balancer pool '{model}' has been disabled by an administrator"
        )))
    } else {
        crate::api::routes::guard_model_substitution(state, model)?;
        Ok(state.router.resolve(model))
    }
}

/// Session stickiness — pin this session to the resolved provider. Callers
/// pass `session_id = None` for a bound request: the variant already fixed
/// the model, and a pin left behind would steer the session's unbound
/// requests to it.
fn apply_session_affinity(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    session_id: Option<&str>,
    provider_name: String,
    canonical_model: String,
) -> (String, String) {
    let Some(session_id) = session_id else {
        return (provider_name, canonical_model);
    };
    use crate::router::session_affinity::resolve_with_pin;
    let pin = if should_skip_affinity(headers) {
        None
    } else {
        state.session_affinity.get(session_id)
    };
    let (pinned_provider, pinned_model, should_update) =
        resolve_with_pin(pin.as_ref(), &provider_name, &canonical_model);
    if should_update {
        state
            .session_affinity
            .set(session_id, &pinned_provider, &pinned_model);
    }
    (pinned_provider, pinned_model)
}

/// Serve the request from the response cache if it holds an entry for `key`.
/// Returns the finished response (metered as a saving via `record_cache_hit`)
/// or `None` on a miss.
#[allow(clippy::too_many_arguments)]
async fn try_serve_cached_completion(
    state: &AppState,
    key: &str,
    request_model: &str,
    canonical_model: &str,
    provider_name: &str,
    user: &crate::db::models::User,
    attribution: &crate::api::attribution::Attribution,
    body: &Value,
    skip_log: bool,
) -> Option<Response> {
    let cached = state.response_cache.get_completion(key, canonical_model).await?;
    tracing::info!(
        cache_key = key,
        model = canonical_model,
        "response cache hit"
    );
    // What the call would have cost. Recorded as a saving, not as spend.
    let avoided_cost = state.cost_calc.calculate_with_cache(
        canonical_model,
        cached.prompt_tokens,
        cached.completion_tokens,
        cached.cache_read_tokens,
        cached.cache_write_tokens,
    );
    record_cache_hit(
        state,
        CacheHitCtx {
            user_id: user.id,
            api_key_id: user.api_key_id,
            user_project: attribution.project_or(user.api_key_project.clone()),
            request_model: request_model.to_string(),
            canonical_model: canonical_model.to_string(),
            provider: provider_name.to_string(),
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
        Json(build_openai_response(request_id, canonical_model, &cached)).into_response();
    response
        .headers_mut()
        .insert(CACHE_HEADER, axum::http::HeaderValue::from_static("HIT"));
    Some(response)
}

/// What `complete_with_retry_and_fallback` settled on: the completion plus the
/// provider/model pair that actually answered (after any fallback hops) and
/// how many provider calls it took.
struct ProviderCallOutcome {
    result: crate::providers::adapter::CompletionResult,
    provider: String,
    model: String,
    /// Provider calls made for this request: first try, backoff retries and
    /// failover hops that reached a provider all count; a circuit-breaker skip
    /// does not (no provider was called). 1 = first-try success.
    attempts: i64,
}

/// Call the provider with backoff retries, walking the fallback chain on
/// non-retryable failures. No fallback for a bound request: the pinned model
/// failing is the experiment's result, and a substitute answering would be
/// recorded against the variant that did not answer.
async fn complete_with_retry_and_fallback(
    state: &AppState,
    user: &crate::db::models::User,
    body: &Value,
    bound: bool,
    provider_name: String,
    canonical_model: String,
) -> Result<ProviderCallOutcome, ApiError> {
    let retry_policy = crate::router::retry::RetryPolicy::from_config(&state.settings.retry);
    let mut current_model = canonical_model;
    let mut current_provider = provider_name;
    let mut attempts: i64 = 0;
    // A tools request must never fall back onto an adapter that would drop
    // the tools (issue #88): the substitute would answer in prose.
    let require_tools = request_has_tools(body);
    let result = loop {
        if state.circuit_breaker.is_open(&current_provider) {
            tracing::warn!(provider = current_provider.as_str(), "circuit breaker open, skipping provider");
            let pseudo_err = anyhow::anyhow!("circuit breaker open for {}", current_provider);
            if bound {
                return Err(ApiError::ProviderError(pseudo_err));
            }
            match next_available_fallback_with_policy(state, user, &current_model, require_tools).await {
                Some((next_provider, next_canonical)) => {
                    current_model = next_canonical;
                    current_provider = next_provider;
                    continue;
                }
                None => {
                    return Err(ApiError::ProviderError(pseudo_err));
                }
            }
        }
        let adapter = state
            .provider_registry
            .get(&current_provider)
            .map_err(ApiError::ProviderError)?;
        let call_result = call_with_backoff(
            &retry_policy,
            &mut attempts,
            &current_provider,
            || {
                let req = build_normalized_request(
                    body,
                    current_model.clone(),
                    &state.settings.model_capabilities,
                );
                let adapter = adapter.clone();
                async move { adapter.complete(&req).await }.instrument(tracing::info_span!(
                    "modelrouter.provider_call",
                    "provider.name" = current_provider.as_str()
                ))
            },
        )
        .await;
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
                if bound {
                    return Err(ApiError::ProviderError(e));
                }
                match next_available_fallback_with_policy(state, user, &current_model, require_tools).await {
                    Some((next_provider, next_canonical)) => {
                        current_model = next_canonical;
                        current_provider = next_provider;
                        tracing::info!(fallback_model = current_model.as_str(), "Retrying with fallback");
                    }
                    None => {
                        return Err(ApiError::ProviderError(e));
                    }
                }
            }
        }
    };
    Ok(ProviderCallOutcome {
        result,
        provider: current_provider,
        model: current_model,
        attempts,
    })
}

/// One provider's retry loop: call, classify the error, back off and retry
/// while the policy allows. Each call made increments `attempts`.
async fn call_with_backoff<F, Fut>(
    retry_policy: &crate::router::retry::RetryPolicy,
    attempts: &mut i64,
    provider: &str,
    mut call: F,
) -> anyhow::Result<crate::providers::adapter::CompletionResult>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<crate::providers::adapter::CompletionResult>>,
{
    let mut retry_attempt = 0u32;
    loop {
        *attempts += 1;
        match call().await {
            Ok(r) => return Ok(r),
            Err(e) => {
                let err_str = e.to_string();
                let retryable = crate::router::retry::RetryableError::classify(&err_str);
                if retry_policy.should_retry(retry_attempt, &retryable) {
                    let delay = retry_policy.delay_ms(retry_attempt);
                    tracing::warn!(
                        attempt = retry_attempt,
                        delay_ms = delay,
                        provider = provider,
                        error = %err_str,
                        "provider error, retrying with backoff"
                    );
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                    retry_attempt += 1;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// Success-path metric recording for the non-streaming completion.
#[allow(unused_variables)]
fn record_success_metrics(
    state: &AppState,
    model: &str,
    provider: &str,
    user_id: i64,
    result: &crate::providers::adapter::CompletionResult,
    cost: f64,
    latency_ms: i64,
) {
    #[cfg(feature = "otel")]
    {
        crate::telemetry::metrics::record_request(model, provider, "ok");
        crate::telemetry::metrics::record_tokens(
            model,
            provider,
            result.prompt_tokens,
            result.completion_tokens,
        );
        crate::telemetry::metrics::record_cost(model, provider, user_id, cost);
        crate::telemetry::metrics::record_duration(model, provider, false, latency_ms as f64);
    }

    #[cfg(feature = "prometheus")]
    if let Some(ref metrics) = state.app_metrics {
        metrics.record_request(model, provider, "ok");
        metrics.record_tokens(model, provider, result.prompt_tokens, result.completion_tokens);
        metrics.record_cost(model, provider, cost);
    }
}

/// Everything the fire-and-forget logging task needs, owned, so the spawned
/// task borrows nothing from the handler.
struct CompletionLogCtx {
    state: AppState,
    user_id: i64,
    api_key_id: Option<i64>,
    user_project: Option<String>,
    user_name: String,
    /// What the rows record as the request model (the caller's name under a binding).
    model: String,
    canonical_model: String,
    provider: String,
    messages_json: String,
    response: String,
    finish_reason: String,
    prompt_tokens: u32,
    completion_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
    ttft_ms: Option<i64>,
    attempts: i64,
    cost: f64,
    latency_ms: i64,
    /// See `write_prompt` in the handler: the prompt-row gate, with a
    /// retaining experiment already folded in.
    write_prompt: bool,
    skip_log: bool,
    /// The policy the prompt row is redacted under (content storage forced on
    /// for a retaining binding), fixed at request time.
    effective_storage: StorageConfig,
    attribution_correlation_id: Option<String>,
    attribution_tags: String,
    experiment_id: Option<i64>,
    experiment_variant: Option<String>,
}

/// Fire-and-forget logging for a non-streaming completion: the prompt row
/// (when `write_prompt`), the cost-ledger row, callback egress (unless
/// `skip_log`) and `on_response_sent` lifecycle hooks.
fn spawn_completion_logging(ctx: CompletionLogCtx) {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    tokio::spawn(async move {
        let ledger_row = |prompt_id: Option<i64>| NewCostLedgerEntry {
            user_id: ctx.user_id,
            prompt_id,
            model: ctx.canonical_model.clone(),
            provider: ctx.provider.clone(),
            project: ctx.user_project.clone(),
            tokens_in: ctx.prompt_tokens as i64,
            tokens_out: ctx.completion_tokens as i64,
            cost_usd: ctx.cost,
            api_key_id: ctx.api_key_id,
            attribution_correlation_id: ctx.attribution_correlation_id.clone(),
            attribution_tags: ctx.attribution_tags.clone(),
            experiment_id: ctx.experiment_id,
            experiment_variant: ctx.experiment_variant.clone(),
            tokens_estimated: false,
        };

        if ctx.write_prompt {
            let mut prompt = NewPrompt {
                user_id: ctx.user_id,
                session_id: None,
                request_model: ctx.model.clone(),
                routed_model: ctx.canonical_model.clone(),
                provider: ctx.provider.clone(),
                messages: ctx.messages_json.clone(),
                response: Some(ctx.response.clone()),
                finish_reason: Some(ctx.finish_reason.clone()),
                prompt_tokens: ctx.prompt_tokens as i64,
                completion_tokens: ctx.completion_tokens as i64,
                cache_read_tokens: ctx.cache_read_tokens as i64,
                cache_write_tokens: ctx.cache_write_tokens as i64,
                cost_usd: ctx.cost,
                latency_ms: Some(ctx.latency_ms),
                ttft_ms: ctx.ttft_ms,
                attempts: Some(ctx.attempts),
                tags: "[]".to_string(),
                project: ctx.user_project.clone(),
                attribution_correlation_id: ctx.attribution_correlation_id.clone(),
                attribution_tags: ctx.attribution_tags.clone(),
                experiment_id: ctx.experiment_id,
                experiment_variant: ctx.experiment_variant.clone(),
            };
            crate::db::prompt_store::redact_prompt_content(&ctx.effective_storage, &mut prompt);
            match PromptRepository::create(&*ctx.state.prompt_db, prompt).await {
                Ok(saved_prompt) => {
                    if let Err(e) =
                        CostRepository::create(&*ctx.state.db, ledger_row(Some(saved_prompt.id))).await
                    {
                        tracing::error!("Failed to record cost: {}", e);
                    }
                    // A row written only because a retaining experiment asked
                    // for it does not open the operator's egress gate.
                    if !ctx.skip_log {
                        let mut event = crate::callbacks::CallbackEvent {
                            trace_id: format!("{}", saved_prompt.id),
                            user_id: ctx.user_id,
                            model: ctx.canonical_model.clone(),
                            provider: ctx.provider.clone(),
                            input: serde_json::from_str(&ctx.messages_json)
                                .unwrap_or(serde_json::Value::Null),
                            output: ctx.response.clone(),
                            prompt_tokens: ctx.prompt_tokens,
                            completion_tokens: ctx.completion_tokens,
                            cost_usd: ctx.cost,
                            latency_ms: ctx.latency_ms,
                        };
                        // The row above was redacted; the egress must be too (issue #53).
                        crate::db::prompt_store::redact_callback_content(
                            &ctx.state.storage.load(),
                            &mut event,
                        );
                        ctx.state.callbacks.dispatch(event);
                    }
                }
                Err(e) => tracing::error!("Failed to record prompt: {}", e),
            }
        } else {
            // Skip logging but still record cost for budget enforcement
            if let Err(e) = CostRepository::create(&*ctx.state.db, ledger_row(None)).await {
                tracing::error!("Failed to record cost: {}", e);
            }
        }

        // Fire on_response_sent lifecycle hooks
        for hook in &ctx.state.settings.hooks.lifecycle {
            if hook.event == "on_response_sent" {
                let payload = crate::hooks::lifecycle::response_sent_payload(
                    &ctx.user_name,
                    &ctx.model,
                    &ctx.canonical_model,
                    ctx.cost,
                    ctx.latency_ms,
                );
                crate::hooks::lifecycle::fire(hook, payload);
            }
        }
    });
}

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
/// Next fallback candidate after `current_model` that passes policy model-permission
/// checks and is not operator-disabled. A candidate denied by policy or disabled by
/// an administrator is skipped, not fatal — the chain exists to find a working
/// alternative. Bounded by MAX_FALLBACK_HOPS so a looping chain terminates.
async fn next_available_fallback_with_policy(
    state: &AppState,
    user: &crate::db::models::User,
    current_model: &str,
    require_tools: bool,
) -> Option<(String, String)> {
    const MAX_FALLBACK_HOPS: usize = 16;

    let mut cursor = current_model.to_string();
    for _ in 0..MAX_FALLBACK_HOPS {
        let next_model = state.fallback.next_after(&cursor)?;
        let (next_provider, next_canonical) = state.router.resolve(&next_model);

        // Check operator availability first
        if !state.router.is_available(&next_provider, &next_canonical) {
            tracing::info!(
                skipped_model = next_model.as_str(),
                "fallback candidate is disabled by an administrator, trying the next one"
            );
            cursor = next_model;
            continue;
        }

        // A request carrying tools can only fall back to an adapter that
        // forwards them (issue #88); anything else would silently drop the
        // caller's tools mid-conversation.
        if require_tools {
            let forwards_tools = state
                .provider_registry
                .get(&next_provider)
                .map(|a| a.supports_tools(&next_canonical))
                .unwrap_or(false);
            if !forwards_tools {
                tracing::info!(
                    skipped_model = next_model.as_str(),
                    "fallback candidate does not support tools, trying the next one"
                );
                cursor = next_model;
                continue;
            }
        }

        // Check policy model permissions (no rate-limit increment, no budget sum)
        match state.policy.model_permitted_denial(user, &next_canonical).await {
            Ok(None) => {
                // Permitted
                return Some((next_provider, next_canonical));
            }
            Ok(Some(reason)) => {
                // Policy denies this model for this user — skip it
                tracing::warn!(
                    model = next_canonical.as_str(),
                    user_id = user.id,
                    reason = reason.as_str(),
                    "fallback candidate denied by policy, trying the next one"
                );
                cursor = next_model;
            }
            Err(e) => {
                // Policy engine error — fail closed for this candidate (skip it)
                tracing::warn!(
                    model = next_canonical.as_str(),
                    error = %e,
                    "policy check error for fallback candidate, skipping"
                );
                cursor = next_model;
            }
        }
    }
    None
}

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
            experiment_id: None,
            experiment_variant: None,
            tokens_estimated: false,
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
                // The cached result carries the *original* call's TTFT, which
                // says nothing about this request. Never persisted on a hit;
                // likewise no attempts — no provider was called.
                ttft_ms: None,
                attempts: None,
                tags: "[]".to_string(),
                project: ctx.user_project.clone(),
                attribution_correlation_id: ctx.attribution.correlation_id.clone(),
                attribution_tags: ctx.attribution.tags_json(),
                experiment_id: None,
                experiment_variant: None,
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

#[derive(Clone)]
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
    /// See `write_prompt` in the handler: the prompt-row gate, with a
    /// retaining experiment already folded in.
    write_prompt: bool,
    /// The policy the prompt row is redacted under (content storage forced on
    /// for a retaining binding), fixed at request time.
    storage: StorageConfig,
    attribution: crate::api::attribution::Attribution,
    experiment_id: Option<i64>,
    experiment_variant: Option<String>,
}

/// Usage as the provider reported it inside a streamed chunk (OpenAI shape:
/// `prompt_tokens` is the whole prompt, `cached_tokens` the subset served from
/// the provider's prompt cache).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportedUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cached_tokens: u32,
}

/// What one SSE chunk contributed to a stream's accounting.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SseChunkInfo {
    /// Delta text from every `data:` line in the chunk, concatenated.
    pub text: String,
    /// The last `usage` object in the chunk, if any line carried one.
    pub usage: Option<ReportedUsage>,
    /// The last non-null `finish_reason` in the chunk.
    pub finish_reason: Option<String>,
    /// Whether the terminal `[DONE]` marker was in this chunk.
    pub done: bool,
}

/// Read everything the ledger cares about out of one SSE chunk. A chunk may
/// carry several `data:` lines (the terminal chunk usually carries the last
/// delta, the usage-bearing chunk and `[DONE]` together), so every line is
/// examined rather than just the first.
pub fn parse_sse_chunk(chunk: &[u8]) -> SseChunkInfo {
    let mut info = SseChunkInfo::default();
    let Ok(text) = std::str::from_utf8(chunk) else {
        return info;
    };
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data.trim() == "[DONE]" {
            info.done = true;
            continue;
        }
        let Ok(json) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Some(content) = json["choices"][0]["delta"]["content"].as_str() {
            info.text.push_str(content);
        }
        if let Some(reason) = json["choices"][0]["finish_reason"].as_str() {
            info.finish_reason = Some(reason.to_string());
        }
        // OpenAI sends `"usage": null` on every chunk but the last when
        // `include_usage` is set; only an object with both counts is usage.
        let usage = &json["usage"];
        if let (Some(prompt), Some(completion)) = (
            usage["prompt_tokens"].as_u64(),
            usage["completion_tokens"].as_u64(),
        ) {
            info.usage = Some(ReportedUsage {
                prompt_tokens: prompt as u32,
                completion_tokens: completion as u32,
                cached_tokens: usage["prompt_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0) as u32,
            });
        }
    }
    info
}

/// Running totals for one stream, folded from each chunk as it passes.
#[derive(Default)]
struct StreamAcc {
    content: String,
    usage: Option<ReportedUsage>,
    finish_reason: Option<String>,
    /// Elapsed time at the first body chunk — the stream's time to first
    /// token, measured from just before the provider dispatch.
    ttft_ms: Option<i64>,
    /// Set once a ledger write has been spawned so the drop guard never writes
    /// a second row for the same stream.
    recorded: bool,
}

/// Token counts and cost settled for one stream, ready to write.
struct StreamSettlement {
    content: String,
    finish_reason: String,
    prompt_tokens: u32,
    completion_tokens: u32,
    cache_read_tokens: u32,
    /// True when the provider never reported usage and the counts above are
    /// the character-count estimate.
    tokens_estimated: bool,
    cost: f64,
    latency_ms: i64,
    ttft_ms: Option<i64>,
}

/// Owns the accounting for one streamed response and writes it exactly once:
/// when `[DONE]` passes, when the provider errors mid-stream, or — via `Drop` —
/// when the body is dropped before either (the client went away, or the
/// provider closed the connection without a terminal chunk). Before the guard
/// an early end left no ledger row at all, so the tokens already streamed were
/// never charged against any budget.
struct StreamLogger {
    ctx: StreamLogCtx,
    acc: StreamAcc,
}

impl StreamLogger {
    fn observe(&mut self, chunk_result: anyhow::Result<bytes::Bytes>) -> anyhow::Result<bytes::Bytes> {
        match &chunk_result {
            Ok(chunk) => {
                if self.acc.ttft_ms.is_none() {
                    self.acc.ttft_ms = Some(self.ctx.start.elapsed().as_millis() as i64);
                }
                let info = parse_sse_chunk(chunk);
                self.acc.content.push_str(&info.text);
                if info.usage.is_some() {
                    self.acc.usage = info.usage;
                }
                if info.finish_reason.is_some() {
                    self.acc.finish_reason = info.finish_reason;
                }
                if info.done {
                    self.record(None);
                }
            }
            Err(e) => self.record(Some(e.to_string())),
        }
        chunk_result
    }

    /// Provider-reported usage when the stream carried it; otherwise the
    /// character-count estimate, flagged as such.
    fn settle(&self, finish_reason: String) -> StreamSettlement {
        let content = self.acc.content.clone();
        let (prompt_tokens, completion_tokens, cache_read_tokens, tokens_estimated) =
            match self.acc.usage {
                Some(u) => (u.prompt_tokens, u.completion_tokens, u.cached_tokens, false),
                None => (
                    (self.ctx.messages_json.chars().count() / 4) as u32,
                    (content.chars().count() / 4) as u32,
                    0,
                    true,
                ),
            };
        // `calculate_with_cache` wants the non-cached share of the prompt;
        // OpenAI's `prompt_tokens` includes the cached tokens.
        let cost = self.ctx.state.cost_calc.calculate_with_cache(
            &self.ctx.canonical_model,
            prompt_tokens.saturating_sub(cache_read_tokens),
            completion_tokens,
            cache_read_tokens,
            0,
        );
        StreamSettlement {
            content,
            finish_reason,
            prompt_tokens,
            completion_tokens,
            cache_read_tokens,
            tokens_estimated,
            cost,
            latency_ms: self.ctx.start.elapsed().as_millis() as i64,
            ttft_ms: self.acc.ttft_ms,
        }
    }

    /// Spawn the ledger write. `provider_error` is set when the stream broke
    /// with an error, which additionally records a failure at stage `provider`
    /// so the break is diagnosable alongside non-streaming provider failures.
    fn record(&mut self, provider_error: Option<String>) {
        if self.acc.recorded {
            return;
        }
        self.acc.recorded = true;

        let finish_reason = match &provider_error {
            Some(_) => "error".to_string(),
            None => self
                .acc
                .finish_reason
                .clone()
                .unwrap_or_else(|| "stop".to_string()),
        };
        let settlement = self.settle(finish_reason);
        let ctx = self.ctx.clone();

        // `Drop` runs wherever the body is released; without a runtime there
        // is nothing to spawn onto, and panicking in Drop would abort.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("streaming ledger write skipped: no tokio runtime");
            return;
        };
        handle.spawn(async move {
            if let Some(message) = provider_error {
                let failure_ctx = crate::api::failure_log::FailureContext {
                    endpoint: "/v1/chat/completions",
                    request_model: ctx.model.clone(),
                    routed_model: Some(ctx.canonical_model.clone()),
                    provider: Some(ctx.provider.clone()),
                    user_id: Some(ctx.user_id),
                    api_key_id: ctx.api_key_id,
                    project: ctx.user_project.clone(),
                    attribution_correlation_id: ctx.attribution.correlation_id.clone(),
                    attribution_tags: ctx.attribution.tags_json(),
                    latency_ms: Some(settlement.latency_ms),
                    experiment_id: ctx.experiment_id,
                    experiment_variant: ctx.experiment_variant.clone(),
                };
                let err = ApiError::ProviderError(anyhow::anyhow!("{message}"));
                crate::api::failure_log::record_failure(&ctx.state, failure_ctx, &err).await;
            }
            write_stream_ledger(ctx, settlement).await;
        });
    }
}

impl Drop for StreamLogger {
    fn drop(&mut self) {
        if !self.acc.recorded {
            self.acc.finish_reason = Some("aborted".to_string());
            self.record(None);
        }
    }
}

/// Persist one streamed response: prompt row (unless logging is skipped),
/// ledger row, lifecycle hooks. Mirrors the non-streaming write.
async fn write_stream_ledger(ctx: StreamLogCtx, s: StreamSettlement) {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let attr_correlation = ctx.attribution.correlation_id.clone();
    let attr_tags = ctx.attribution.tags_json();
    let ledger = NewCostLedgerEntry {
        user_id: ctx.user_id,
        prompt_id: None,
        model: ctx.canonical_model.clone(),
        provider: ctx.provider.clone(),
        project: ctx.user_project.clone(),
        tokens_in: s.prompt_tokens as i64,
        tokens_out: s.completion_tokens as i64,
        cost_usd: s.cost,
        api_key_id: ctx.api_key_id,
        attribution_correlation_id: attr_correlation.clone(),
        attribution_tags: attr_tags.clone(),
        experiment_id: ctx.experiment_id,
        experiment_variant: ctx.experiment_variant.clone(),
        tokens_estimated: s.tokens_estimated,
    };

    let prompt_id = if !ctx.write_prompt {
        // Skip logging but still record cost for budget enforcement
        None
    } else {
        let mut prompt = NewPrompt {
            user_id: ctx.user_id,
            session_id: None,
            request_model: ctx.model.clone(),
            routed_model: ctx.canonical_model.clone(),
            provider: ctx.provider.clone(),
            messages: ctx.messages_json.clone(),
            response: Some(s.content),
            finish_reason: Some(s.finish_reason),
            prompt_tokens: s.prompt_tokens as i64,
            completion_tokens: s.completion_tokens as i64,
            cache_read_tokens: s.cache_read_tokens as i64,
            cache_write_tokens: 0,
            cost_usd: s.cost,
            latency_ms: Some(s.latency_ms),
            ttft_ms: s.ttft_ms,
            // The streaming path has no retry or fallback loop: one dispatch.
            attempts: Some(1),
            tags: "[]".to_string(),
            project: ctx.user_project.clone(),
            attribution_correlation_id: attr_correlation,
            attribution_tags: attr_tags,
            experiment_id: ctx.experiment_id,
            experiment_variant: ctx.experiment_variant.clone(),
        };
        crate::db::prompt_store::redact_prompt_content(&ctx.storage, &mut prompt);
        match PromptRepository::create(&*ctx.state.prompt_db, prompt).await {
            Ok(saved) => Some(saved.id),
            Err(e) => {
                tracing::error!("Failed to log streaming prompt: {}", e);
                None
            }
        }
    };

    let ledger = NewCostLedgerEntry { prompt_id, ..ledger };
    if let Err(e) = CostRepository::create(&*ctx.state.db, ledger).await {
        tracing::error!("Failed to log streaming cost: {}", e);
    }

    // Fire on_response_sent lifecycle hooks
    for hook in &ctx.state.settings.hooks.lifecycle {
        if hook.event == "on_response_sent" {
            let payload = crate::hooks::lifecycle::response_sent_payload(
                &ctx.user_name,
                &ctx.model,
                &ctx.canonical_model,
                s.cost,
                s.latency_ms,
            );
            crate::hooks::lifecycle::fire(hook, payload);
        }
    }
}

/// Wraps an SSE stream so its prompt and cost are recorded in the DB: on the
/// terminal `[DONE]` chunk, on a provider error, or when the body is dropped
/// early. See [`StreamLogger`].
fn log_streaming_request(
    stream: crate::providers::adapter::SseStream,
    ctx: StreamLogCtx,
) -> impl futures::Stream<Item = anyhow::Result<bytes::Bytes>> + Send {
    use futures::StreamExt;

    let mut logger = StreamLogger {
        ctx,
        acc: StreamAcc::default(),
    };
    // The closure owns the logger, so dropping the mapped stream drops the
    // logger and its guard fires.
    stream.map(move |chunk_result| logger.observe(chunk_result))
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

    // Tools ride along only as a pair: a `tool_choice` without tools was
    // rejected at the door, and forwarding one alone would be invalid at the
    // provider (issue #88).
    let tools = body["tools"]
        .as_array()
        .filter(|t| !t.is_empty())
        .cloned();
    let tool_choice = if tools.is_some() {
        body.get("tool_choice").filter(|tc| !tc.is_null()).cloned()
    } else {
        None
    };

    crate::providers::adapter::NormalizedRequest {
        model,
        messages: body["messages"].as_array().cloned().unwrap_or_default(),
        stream: body["stream"].as_bool().unwrap_or(false),
        temperature,
        max_tokens: body["max_tokens"].as_u64().map(|v| v as u32),
        tools,
        tool_choice,
        extra_params: serde_json::Value::Object(Default::default()),
    }
}

fn build_openai_response(
    request_id: String,
    model: &str,
    result: &crate::providers::adapter::CompletionResult,
) -> Value {
    // OpenAI reports `content: null` (not "") on a pure tool-call turn, and
    // several client SDKs branch on exactly that (issue #88).
    let mut message = serde_json::json!({
        "role": "assistant",
        "content": if result.content.is_empty() && result.tool_calls.is_some() {
            Value::Null
        } else {
            Value::String(result.content.clone())
        },
    });
    if let Some(tool_calls) = &result.tool_calls {
        message["tool_calls"] = tool_calls.clone();
    }
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
            "message": message,
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
            ttft_ms: None,
            tool_calls: None,
        };
        let response = build_openai_response(
            "chatcmpl-mr-test".to_string(),
            "gpt-4o-2026-01-01",
            &result,
        );
        assert_eq!(response["model"], "gpt-4o-2026-01-01");
        // A plain text turn keeps string content and no tool_calls key.
        assert_eq!(response["choices"][0]["message"]["content"], "hello");
        assert!(response["choices"][0]["message"].get("tool_calls").is_none());
    }

    #[test]
    fn tool_call_turn_reports_null_content_and_tool_calls(/* issue #88 */) {
        let result = CompletionResult {
            content: String::new(),
            prompt_tokens: 1,
            completion_tokens: 1,
            finish_reason: "tool_calls".to_string(),
            tool_calls: Some(serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"}
            }])),
            ..Default::default()
        };
        let response =
            build_openai_response("chatcmpl-mr-test".to_string(), "m", &result);
        let message = &response["choices"][0]["message"];
        // OpenAI reports content: null on a pure tool-call turn and SDKs
        // branch on exactly that.
        assert!(message["content"].is_null());
        assert_eq!(message["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(response["choices"][0]["finish_reason"], "tool_calls");
    }
}

#[cfg(test)]
mod tools_request_tests {
    use super::{build_normalized_request, request_has_tools, validate_tool_choice_requires_tools};
    use serde_json::json;

    #[test]
    fn empty_or_absent_tools_do_not_count(/* issue #88 */) {
        assert!(!request_has_tools(&json!({"messages": []})));
        assert!(!request_has_tools(&json!({"tools": []})));
        assert!(request_has_tools(&json!({"tools": [{"type": "function"}]})));
    }

    #[test]
    fn tool_choice_without_tools_is_rejected(/* issue #88 */) {
        let body = json!({"tool_choice": "required"});
        assert!(validate_tool_choice_requires_tools(&body).is_err());
        // null and "none" both mean "no tools" and stay accepted.
        assert!(validate_tool_choice_requires_tools(&json!({"tool_choice": null})).is_ok());
        assert!(validate_tool_choice_requires_tools(&json!({"tool_choice": "none"})).is_ok());
        assert!(validate_tool_choice_requires_tools(&json!({})).is_ok());
        // With tools present any tool_choice shape is the provider's problem.
        assert!(validate_tool_choice_requires_tools(
            &json!({"tools": [{"type": "function"}], "tool_choice": "required"})
        )
        .is_ok());
    }

    #[test]
    fn normalized_request_carries_tools_and_tool_choice(/* issue #88 */) {
        let body = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "f"}}],
            "tool_choice": "auto",
        });
        let req = build_normalized_request(&body, "m".to_string(), &[]);
        assert_eq!(req.tools.as_ref().unwrap().len(), 1);
        assert_eq!(req.tool_choice, Some(json!("auto")));
    }

    #[test]
    fn tool_choice_never_rides_without_tools(/* issue #88 */) {
        let body = json!({
            "messages": [],
            "tool_choice": "none",
        });
        let req = build_normalized_request(&body, "m".to_string(), &[]);
        assert!(req.tools.is_none());
        assert!(req.tool_choice.is_none());

        let body = json!({"messages": [], "tools": []});
        let req = build_normalized_request(&body, "m".to_string(), &[]);
        assert!(req.tools.is_none());
    }
}

#[cfg(test)]
mod sse_chunk_tests {
    use super::{parse_sse_chunk, ReportedUsage};

    #[test]
    fn reads_text_usage_finish_reason_and_done_from_one_chunk() {
        let chunk = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}],\"usage\":null}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":2,",
            "\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            "data: [DONE]\n\n",
        );
        let info = parse_sse_chunk(chunk.as_bytes());
        assert_eq!(info.text, "Hello");
        assert_eq!(info.finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            info.usage,
            Some(ReportedUsage { prompt_tokens: 12, completion_tokens: 2, cached_tokens: 4 })
        );
        assert!(info.done);
    }

    #[test]
    fn usage_null_is_not_usage() {
        let chunk = b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}],\"usage\":null}\n\n";
        let info = parse_sse_chunk(chunk);
        assert_eq!(info.usage, None);
        assert!(!info.done);
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
