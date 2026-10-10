use std::time::Instant;

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;
use tracing::Instrument;

use super::router_meta::{CallCost, RouterMeta, TimingMeta};
use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::models::{NewCostLedgerEntry, NewPrompt},
    providers::search::SearchRequest,
    router::policy::PolicyDecision,
};


/// Dollars per query for `search/{engine}`: `[[pricing]]`, then the live price
/// feed (CostCalculator::flat_rate). No fallback: an unpriced engine is
/// recorded at $0, warned about once, and reported `priced: false` so a caller
/// never mistakes it for free.
fn search_rate(state: &AppState, pseudo_model: &str) -> Option<f64> {
    let rate = state.cost_calc.flat_rate(pseudo_model);
    if rate.is_none() {
        state.cost_calc.note_unpriced(pseudo_model);
    }
    rate
}

/// `max_results` is bounded to keep a single request from fanning out into an
/// unbounded (and unbudgeted) amount of provider work.
const MAX_RESULTS_LIMIT: u32 = 20;

/// Result of inferring a search engine when nothing named one explicitly.
/// `Undetermined` carries whatever engines ARE configured (empty means none)
/// so each caller can render its own reason — a 400 to an API caller and a
/// health-probe "skipped" reason read very differently even though they hit
/// the same fork.
pub(crate) enum InferredSearchEngine {
    Determined(String),
    Undetermined { configured: Vec<String> },
}

/// Infer a search engine from config/registry state alone, in precedence
/// order:
///
/// 1. `[routing] default_search_engine` in config.toml,
/// 2. the sole configured search provider, when there is exactly one.
///
/// Shared by `resolve_engine` below (the `/v1/search` request path, which
/// tries the request's own `engine` field first) and the `/health/deep`
/// search probe (where the same defect appeared: the probe used to hardcode
/// `"tavily"` as `search_probe_engine`'s default, so a host configured for
/// Vertex-only search had a probe that tested an engine not actually on the
/// live path — reporting a real outage as healthy, or a healthy Vertex path
/// as down, depending on which engine happened to also be configured). One
/// function means the request path and the probe can never infer
/// differently from the same config again.
///
/// With two or more engines configured and no explicit default, this reports
/// `Undetermined` rather than guessing: picking one would reintroduce
/// exactly the silent-substitution problem `strict_model_resolution` exists
/// to prevent for chat models, just for search engines instead of models.
pub(crate) fn infer_search_engine(state: &AppState) -> InferredSearchEngine {
    if let Some(configured) = state
        .settings
        .routing
        .default_search_engine
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        return InferredSearchEngine::Determined(configured.to_string());
    }

    let available = state.search_registry.configured_engines();
    match available.as_slice() {
        [only] => InferredSearchEngine::Determined(only.clone()),
        _ => InferredSearchEngine::Undetermined { configured: available },
    }
}

/// Decide which engine serves a `/v1/search` request, in precedence order:
///
/// 1. the `engine` field on the request,
/// 2. `[routing] default_search_engine` in config.toml,
/// 3. the sole configured search provider, when there is exactly one.
///
/// This replaced a hardcoded `unwrap_or("tavily")`. That default meant a host
/// configuring only `[providers.vertex]` answered every engine-less request
/// with `502 No search adapter configured for engine: tavily` — a working,
/// reachable adapter sitting unused because the fallback named a provider the
/// operator had never configured. Callers that DO send `engine` were unaffected,
/// so the break was invisible until something omitted the field.
fn resolve_engine(
    state: &AppState,
    requested: Option<&str>,
) -> Result<String, ApiError> {
    if let Some(engine) = requested.map(str::trim).filter(|e| !e.is_empty()) {
        return Ok(engine.to_string());
    }

    match infer_search_engine(state) {
        InferredSearchEngine::Determined(engine) => Ok(engine),
        InferredSearchEngine::Undetermined { configured } if configured.is_empty() => {
            Err(ApiError::InvalidRequest(format!(
                "no search engine configured: add a [providers.<engine>] section \
                 (supported by this build: {}) to config.toml",
                crate::providers::search_registry::supported_engines().join(", ")
            )))
        }
        InferredSearchEngine::Undetermined { configured } => Err(ApiError::InvalidRequest(format!(
            "request omitted `engine` and multiple search engines are configured ({}); \
             send `engine` explicitly or set [routing] default_search_engine",
            configured.join(", ")
        ))),
    }
}

pub async fn search(
    State(state): State<AppState>,
    extensions: Option<axum::Extension<crate::extensions::Extensions>>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!(
        "search",
        user_id = tracing::field::Empty,
        engine = tracing::field::Empty,
        "cost.usd" = tracing::field::Empty,
        "results.count" = tracing::field::Empty,
    );
    let extensions = extensions.map(|axum::Extension(e)| e).unwrap_or_default();
    search_inner(State(state), extensions, user, headers, Json(body))
        .instrument(span)
        .await
}

/// Every rule a request extension applied: a WARN line naming the rule, never
/// the text it matched, and one count on each metrics backend.
fn record_extension_hits(
    #[allow(unused_variables)] state: &AppState,
    ctx: &crate::extensions::RequestContext,
    hits: &[(String, crate::extensions::RuleHit)],
) {
    let project = ctx.project.as_deref().unwrap_or("");
    for (extension, hit) in hits {
        tracing::warn!(
            endpoint = ctx.endpoint,
            extension = extension.as_str(),
            rule = hit.rule.as_str(),
            category = hit.category.as_str(),
            action = hit.action.as_str(),
            project,
            correlation_id = ctx.correlation_id.as_deref().unwrap_or("-"),
            "request extension applied a rule"
        );
        #[cfg(feature = "otel")]
        crate::telemetry::metrics::record_policy_rejected(extension, &hit.rule, &hit.category, &hit.action, project);
        #[cfg(feature = "prometheus")]
        if let Some(ref metrics) = state.app_metrics {
            metrics.record_policy_rejected(extension, &hit.rule, &hit.category, &hit.action, project);
        }
    }
}

/// Execute search with fallback chain. Returns (result, serving_engine, latency_ms).
/// What the fallback walk settled on.
struct SearchOutcome {
    response: crate::providers::search::SearchResponse,
    /// The engine that answered.
    engine: String,
    /// Duration of the answering engine's call.
    latency_ms: i64,
    /// Engine calls made (skipped/unconfigured candidates excluded).
    attempts: i64,
}

async fn execute_search_with_fallback(
    state: &AppState,
    user: &crate::db::models::User,
    engine: &str,
    req: &SearchRequest,
) -> Result<SearchOutcome, ApiError> {
    let chain = state
        .settings
        .routing
        .search_fallback_chains
        .get(engine)
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    let mut engines_to_try = vec![engine.to_string()];
    engines_to_try.extend_from_slice(chain);

    // Dedupe while preserving order (cheap adjacent item #11).
    let mut seen = std::collections::HashSet::new();
    engines_to_try.retain(|e| seen.insert(e.clone()));

    let mut last_error: Option<anyhow::Error> = None;
    let mut attempts: i64 = 0;

    for (idx, candidate) in engines_to_try.iter().enumerate() {
        // Policy re-check for fallback candidates (not the primary, which was
        // already checked). A denial is not fatal — skip the candidate and try
        // the next one. The primary was already permitted by the route's check.
        if idx > 0 {
            let candidate_pseudo_model = format!("search/{}", candidate);
            match state.policy.model_permitted_denial(user, &candidate_pseudo_model).await {
                Ok(None) => {
                    // Permitted
                }
                Ok(Some(reason)) => {
                    tracing::warn!(
                        engine = candidate,
                        user_id = user.id,
                        reason = reason.as_str(),
                        "search fallback candidate denied by policy, trying the next one"
                    );
                    continue;
                }
                Err(e) => {
                    // Policy engine error — fail closed for this candidate (skip it)
                    tracing::warn!(
                        engine = candidate,
                        error = %e,
                        "policy check error for search fallback candidate, skipping"
                    );
                    continue;
                }
            }
        }

        if !crate::providers::search_registry::is_supported_engine(candidate) {
            tracing::debug!(
                engine = candidate,
                "skipping unsupported engine in fallback chain"
            );
            continue;
        }

        let adapter = match state.search_registry.get(candidate) {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(
                    engine = candidate,
                    error = %e,
                    "skipping unconfigured engine in fallback chain"
                );
                continue;
            }
        };

        let start = Instant::now();
        attempts += 1;
        match adapter.search(req).await {
            Ok(response) => {
                return Ok(SearchOutcome {
                    response,
                    engine: candidate.clone(),
                    latency_ms: start.elapsed().as_millis() as i64,
                    attempts,
                });
            }
            Err(e) => {
                // Classify the error: client errors (4xx except 429) surface
                // immediately without failover; provider errors (5xx, 429,
                // timeouts, connection failures) walk the chain.
                use crate::router::retry::RetryableError;
                let err_str = e.to_string();
                let classified = RetryableError::classify(&err_str);

                if matches!(classified, RetryableError::ClientError(_)) {
                    // Client error (provider 400/404/422 etc) — the caller sent
                    // a bad query/params. Surface immediately, do not fail over.
                    tracing::debug!(
                        engine = candidate,
                        error = %e,
                        "client error from search engine, not failing over"
                    );
                    return Err(ApiError::ProviderError(e));
                }

                // Provider error — try the next engine in the chain.
                let remaining: Vec<_> = engines_to_try[idx + 1..].to_vec();
                last_error = Some(e);
                tracing::warn!(
                    engine = candidate,
                    error = %last_error.as_ref().unwrap(),
                    remaining = ?remaining,
                    "search engine failed, trying next in chain"
                );
            }
        }
    }

    let tried: Vec<_> = engines_to_try.iter().map(|s| s.as_str()).collect();
    Err(ApiError::ProviderError(
        last_error
            .map(|e| anyhow::anyhow!("all engines exhausted (tried: {}): {}", tried.join(", "), e))
            .unwrap_or_else(|| {
                anyhow::anyhow!(
                    "all engines in the fallback chain are unconfigured or unsupported (tried: {})",
                    tried.join(", ")
                )
            }),
    ))
}

async fn search_inner(
    State(state): State<AppState>,
    extensions: crate::extensions::Extensions,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    // Request receipt: `x_router.timing.total_ms` is measured from here.
    let received = Instant::now();
    crate::api::routes::reject_experiment_header("/v1/search", &headers)?;
    let user = user.0;
    tracing::Span::current().record("user_id", user.id);

    let attribution = crate::api::attribution::Attribution::extract(&body, &headers)?;
    let cache_directives = crate::router::cache::CacheDirectives::from_headers(&headers)
        .map_err(ApiError::InvalidRequest)?;

    let mut query = body["query"].as_str().unwrap_or("").to_string();
    if query.trim().is_empty() {
        return Err(ApiError::InvalidRequest(
            "query must not be empty".to_string(),
        ));
    }

    let max_results = match body.get("max_results").and_then(|v| v.as_u64()) {
        Some(0) => {
            return Err(ApiError::InvalidRequest(
                "max_results must be at least 1".to_string(),
            ))
        }
        Some(n) if n > MAX_RESULTS_LIMIT as u64 => {
            return Err(ApiError::InvalidRequest(format!(
                "max_results must be at most {}",
                MAX_RESULTS_LIMIT
            )))
        }
        Some(n) => Some(n as u32),
        None => None,
    };

    // Opt-in: a grounding engine's generated answer is returned only when the
    // caller asks, so existing callers keep the citations-only shape.
    let include_answer = match body.get("include_answer") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(ApiError::InvalidRequest(
                "include_answer must be a boolean".to_string(),
            ))
        }
    };

    let mut research = parse_research_options(&body, include_answer)?;

    let engine = resolve_engine(&state, body["engine"].as_str())?;
    if !crate::providers::search_registry::is_supported_engine(&engine) {
        return Err(ApiError::InvalidRequest(format!(
            "unsupported search engine: {}",
            engine
        )));
    }

    tracing::Span::current().record("engine", engine.as_str());

    // Policy check — engines are gated as pseudo-models "search/{engine}" so
    // allow-lists and budgets apply the same way they do to chat/embedding models.
    // NOTE: Policy is checked against the REQUESTED engine; cost is resolved after
    // failover from the SERVING engine (issue #42).
    let requested_pseudo_model = format!("search/{}", engine);
    let policy_result = state
        .policy
        .check(&user, &requested_pseudo_model)
        .await
        .map_err(|_| ApiError::Internal)?;
    match policy_result {
        PolicyDecision::Allow { .. } => {}
        PolicyDecision::Deny { reason, status, .. } => {
            return Err(ApiError::PolicyDenied { reason, status });
        }
    }

    // ── Request extensions ───────────────────────────────────────────────────
    // Before the cache and the provider, so a rewritten query is what is
    // cached and sent, and a refused one is neither.
    let mut extension_verdict: Option<crate::extensions::SearchVerdict> = None;
    if !extensions.is_empty() {
        let ctx = crate::extensions::RequestContext {
            endpoint: "/v1/search",
            user_id: user.id,
            api_key_id: user.api_key_id,
            project: attribution.project_or(user.api_key_project.clone()),
            correlation_id: attribution.correlation_id.clone(),
            tags: attribution.tags.clone(),
        };
        let mut text = crate::extensions::SearchText {
            query: std::mem::take(&mut query),
            instructions: research.instructions.take(),
            context: research.context.take(),
        };
        let verdict = extensions.on_search(&ctx, &mut text).await;
        record_extension_hits(&state, &ctx, &verdict.hits);
        if let Some((extension, refusal)) = verdict.refusal.clone() {
            let err = ApiError::Refused { status: refusal.status, body: refusal.body, reason: refusal.reason };
            let mut failure = crate::api::failure_log::context_from_request("/v1/search", &state, &user, &headers, &body);
            failure.request_model = requested_pseudo_model.clone();
            failure.provider = Some(format!("extension:{extension}"));
            failure.project = ctx.project.clone();
            failure.latency_ms = Some(received.elapsed().as_millis() as i64);
            crate::api::failure_log::record_failure(&state, failure, &err).await;
            return Err(err);
        }
        query = text.query;
        research.instructions = text.instructions;
        research.context = text.context;
        extension_verdict = Some(verdict);
        if query.trim().is_empty() {
            return Err(ApiError::InvalidRequest(
                "query is empty after request extensions rewrote it".to_string(),
            ));
        }
    }

    // ── Response cache ───────────────────────────────────────────────────────
    // Search queries are deterministic enough to cache by default; the key is
    // engine + query + options, and the TTL is shorter than for completions.
    let cache_plan = if state.policy.cache_enabled(&user, &requested_pseudo_model) {
        state.response_cache.search_plan(cache_directives.mode)
    } else {
        crate::router::cache::CachePlan::Skip
    };
    let cache_key = cache_plan
        .store()
        .then(|| crate::router::cache::search_cache_key(&engine, &query, max_results, &research.cache_fields(include_answer)));

    if let (true, Some(key)) = (cache_plan.lookup(), cache_key.as_ref()) {
        if let Some(payload) = state
            .response_cache
            .get_search(key, &requested_pseudo_model, &cache_directives)
            .await
        {
            // Cache hit: the cached payload already names the serving engine.
            let cached_engine = payload["engine"].as_str().unwrap_or(&engine).to_string();
            let cached_engine = cached_engine.as_str();
            let cached_pseudo_model = format!("search/{}", cached_engine);
            tracing::info!(engine = cached_engine, "search cache hit");
            let results_returned = payload["results"].as_array().map(|r| r.len()).unwrap_or(0) as i64;

            // Recompute cost from the serving engine's pricing
            let rate = search_rate(&state, &cached_pseudo_model);
            let cost = rate.unwrap_or(0.0);

            record_search_cache_hit(
                &state,
                &user,
                &cached_pseudo_model,
                cached_engine,
                results_returned,
                cost,
                &attribution,
            );
            let mut body = payload;
            // The ledger's model for this row: the serving engine's pseudo-model.
            body["model"] = Value::String(cached_pseudo_model.clone());
            let meta = RouterMeta {
                requested_model: requested_pseudo_model.clone(),
                model: cached_pseudo_model.clone(),
                provider: cached_engine.to_string(),
                settings: search_settings(max_results),
                tokens: None,
                results: Some(results_returned),
                cost: CallCost::cache_hit(cost).priced_if(rate.is_some()),
                timing: TimingMeta {
                    total_ms: received.elapsed().as_millis() as i64,
                    ..TimingMeta::default()
                },
            };
            body["usage"] = search_usage(results_returned, meta.cost);
            if let Some(verdict) = &extension_verdict {
                verdict.annotate(&mut body);
            }
            meta.attach(&mut body);
            let mut response = Json(body).into_response();
            response.headers_mut().insert(
                crate::api::routes::completions::CACHE_HEADER,
                axum::http::HeaderValue::from_static("HIT"),
            );
            return Ok(response);
        }
    }

    // ── Fallback chain walk ──────────────────────────────────────────────────
    // Try the primary engine, then walk the chain on provider errors (adapter/
    // provider failures: timeouts, 5xx, rate-limit, connection issues). Do NOT
    // fail over on caller errors (invalid query → 400). Skip chain entries that
    // are unsupported/unconfigured. Return the last error if the chain exhausts.
    let req = SearchRequest {
        query: query.clone(),
        max_results,
        include_answer,
        instructions: research.instructions,
        context: research.context,
        max_follow_up_queries: research.max_follow_up_queries,
    };

    let SearchOutcome {
        response: result,
        engine: serving_engine,
        latency_ms,
        attempts,
    } = execute_search_with_fallback(&state, &user, &engine, &req).await?;

    let results_returned = result.results.len() as i64;

    // Recompute cost and pseudo-model from the SERVING engine (issue #42).
    // Pricing follows the images.rs precedent: `input_per_million` is reused as
    // a flat per-unit dollar rate (here: dollars per query), not a
    // per-million-token rate. See config.example.toml for the documented unit.
    let serving_pseudo_model = format!("search/{}", serving_engine);
    let rate = search_rate(&state, &serving_pseudo_model);
    let cost = rate.unwrap_or(0.0);

    let span = tracing::Span::current();
    span.record("cost.usd", cost);
    span.record("results.count", results_returned);

    // Metrics and cost tracking use the serving_engine, not the requested engine.
    #[cfg(feature = "otel")]
    {
        crate::telemetry::metrics::record_request(&serving_pseudo_model, &serving_engine, "ok");
        crate::telemetry::metrics::record_cost(&serving_pseudo_model, &serving_engine, user.id, cost);
        crate::telemetry::metrics::record_duration(
            &serving_pseudo_model,
            &serving_engine,
            false,
            latency_ms as f64,
        );
    }

    #[cfg(feature = "prometheus")]
    if let Some(ref metrics) = state.app_metrics {
        metrics.record_request(&serving_pseudo_model, &serving_engine, "ok");
        metrics.record_cost(&serving_pseudo_model, &serving_engine, cost);
    }

    // Fire-and-forget cost recording
    let state_clone = state.clone();
    let engine_clone = serving_engine.clone();
    let serving_pseudo_model_clone = serving_pseudo_model.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    let user_project = attribution.project_or(user.api_key_project.clone());
    let attr_correlation = attribution.correlation_id.clone();
    let attr_tags = attribution.tags_json();

    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model: serving_pseudo_model_clone.clone(),
            routed_model: serving_pseudo_model_clone.clone(),
            provider: engine_clone.clone(),
            messages: "[]".to_string(), // search has no chat messages
            response: None,
            finish_reason: None,
            prompt_tokens: results_returned,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: cost,
            latency_ms: Some(latency_ms),
            ttft_ms: None,
            attempts: Some(attempts),
            tags: "[]".to_string(),
            project: user_project.clone(),
            attribution_correlation_id: attr_correlation.clone(),
            attribution_tags: attr_tags.clone(),
            experiment_id: None,
            experiment_variant: None,
        };
        // Storage policy (issue #4 gap, closed in #29): prompt row optional, cost row not.
        let stored = match crate::db::prompt_store::apply_storage_policy(&state_clone.storage.load(), prompt) {
            Some(p) => match PromptRepository::create(&*state_clone.prompt_db, p).await {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::error!("Failed to record search prompt: {}", e);
                    None
                }
            },
            None => None,
        };
        {
                let ledger = NewCostLedgerEntry {
                    user_id,
                    prompt_id: stored.as_ref().map(|s| s.id),
                    model: serving_pseudo_model_clone,
                    provider: engine_clone,
                    project: user_project.clone(),
                    tokens_in: results_returned,
                    tokens_out: 0,
                    cost_usd: cost,
                    api_key_id,
                    attribution_correlation_id: attr_correlation.clone(),
                    attribution_tags: attr_tags.clone(),
                    experiment_id: None,
                    experiment_variant: None,
                    tokens_estimated: false,
                };
                if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
                    tracing::error!("Failed to record search cost: {}", e);
                }
        }
    });

    let meta = RouterMeta {
        requested_model: requested_pseudo_model,
        model: serving_pseudo_model.clone(),
        provider: serving_engine.clone(),
        settings: search_settings(max_results),
        tokens: None,
        results: Some(results_returned),
        cost: CallCost::spent(cost).priced_if(rate.is_some()),
        timing: TimingMeta {
            total_ms: received.elapsed().as_millis() as i64,
            latency_ms,
            provider_ms: Some(latency_ms),
            ttft_ms: None,
            attempts,
            fallbacks: i64::from(serving_engine != engine),
        },
    };
    let mut payload = serde_json::json!({
        "engine": serving_engine,
        // What the ledger row records as the model: the serving engine's
        // pseudo-model, so a caller can attribute the cost to what served it.
        "model": serving_pseudo_model,
        "results": result.results,
        "usage": search_usage(results_returned, meta.cost),
    });
    // Absent, not null, when there is no answer: an engine that cannot produce
    // one answers an `include_answer` request with the plain shape.
    if let Some(answer) = &result.answer {
        payload["answer"] = serde_json::json!(answer);
    }

    // Cached without `x_router`: that describes this call, and a hit
    // replaces it with its own.
    if let Some(key) = cache_key {
        state
            .response_cache
            .put_search(
                &key,
                &serving_pseudo_model,
                payload.clone(),
                cost,
                &cache_directives,
            )
            .await;
    }

    // After the cache write: these describe this request, not the result.
    if let Some(verdict) = &extension_verdict {
        verdict.annotate(&mut payload);
    }
    meta.attach(&mut payload);
    let mut response = Json(payload).into_response();
    // No header when the cache was not involved: that is a plain response,
    // not a miss.
    if let Some(outcome) = cache_plan.miss_header() {
        response.headers_mut().insert(
            crate::api::routes::completions::CACHE_HEADER,
            axum::http::HeaderValue::from_static(outcome),
        );
    }
    Ok(response)
}

/// Grounded-research options on a search request. Each refines the generated
/// answer, so each needs `include_answer: true`: sent without it, the caller
/// would be silently ignored.
#[derive(Debug, Default)]
struct ResearchOptions {
    instructions: Option<String>,
    context: Option<String>,
    max_follow_up_queries: u32,
}

impl ResearchOptions {
    /// What of the request shapes the answer, for the response-cache key.
    fn cache_fields(&self, include_answer: bool) -> crate::router::cache::SearchAnswerKey<'_> {
        crate::router::cache::SearchAnswerKey {
            include_answer,
            instructions: self.instructions.as_deref(),
            context: self.context.as_deref(),
            max_follow_up_queries: self.max_follow_up_queries,
        }
    }
}

fn parse_research_options(body: &Value, include_answer: bool) -> Result<ResearchOptions, ApiError> {
    let text_field = |name: &str| -> Result<Option<String>, ApiError> {
        match body.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(ApiError::InvalidRequest(format!("{name} must be a string"))),
        }
    };
    let options = ResearchOptions {
        instructions: text_field("instructions")?,
        context: text_field("context")?,
        max_follow_up_queries: match body.get("max_follow_up_queries") {
            None | Some(Value::Null) => 0,
            Some(v) => match v.as_u64() {
                Some(n) if n <= u64::from(crate::providers::search::MAX_FOLLOW_UP_QUERIES) => n as u32,
                _ => {
                    return Err(ApiError::InvalidRequest(format!(
                        "max_follow_up_queries must be an integer from 0 to {}",
                        crate::providers::search::MAX_FOLLOW_UP_QUERIES
                    )))
                }
            },
        },
    };
    let refines_answer =
        options.instructions.is_some() || options.context.is_some() || options.max_follow_up_queries > 0;
    if refines_answer && !include_answer {
        return Err(ApiError::InvalidRequest(
            "instructions, context and max_follow_up_queries shape the generated answer and need \
             include_answer: true"
                .to_string(),
        ));
    }
    Ok(options)
}

/// The search `usage` object: results returned plus the ledger cost fields.
fn search_usage(results_returned: i64, cost: CallCost) -> Value {
    let mut usage = serde_json::json!({ "results": results_returned });
    cost.write_into(&mut usage);
    usage
}

/// `x_router.settings` for a search: the options the engine was called with.
fn search_settings(max_results: Option<u32>) -> Value {
    serde_json::json!({ "max_results": max_results })
}

/// Meter a search cache hit: one usage row with `cache_hit = true`, zero cost,
/// and the avoided per-query price recorded as the saving.
fn record_search_cache_hit(
    state: &AppState,
    user: &crate::db::models::User,
    pseudo_model: &str,
    engine: &str,
    results_returned: i64,
    avoided_cost: f64,
    attribution: &crate::api::attribution::Attribution,
) {
    use crate::db::repositories::costs::CostRepository;

    let state = state.clone();
    let ledger = NewCostLedgerEntry {
        user_id: user.id,
        prompt_id: None,
        model: pseudo_model.to_string(),
        provider: engine.to_string(),
        // Same project resolution as the live-call row, so a hit is
        // attributed to the caller's declared project, not only the key's.
        project: attribution.project_or(user.api_key_project.clone()),
        tokens_in: results_returned,
        tokens_out: 0,
        // Interpreted as the avoided cost by `create_cache_hit`.
        cost_usd: avoided_cost,
        api_key_id: user.api_key_id,
        attribution_correlation_id: attribution.correlation_id.clone(),
        attribution_tags: attribution.tags_json(),
        experiment_id: None,
        experiment_variant: None,
        tokens_estimated: false,
    };
    tokio::spawn(async move {
        if let Err(e) = CostRepository::create_cache_hit(&*state.db, ledger).await {
            tracing::error!("Failed to record search cache-hit usage: {}", e);
        }
    });
}
