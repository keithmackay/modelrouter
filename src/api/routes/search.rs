use std::time::Instant;

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;
use tracing::Instrument;

use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::models::{NewCostLedgerEntry, NewPrompt},
    providers::search::SearchRequest,
    router::policy::PolicyDecision,
};

/// Fallback per-query price (USD) used when no `[[pricing]]` entry matches
/// `search/{engine}`. Roughly Tavily's standard-tier list price.
const DEFAULT_COST_PER_QUERY: f64 = 0.005;

/// `max_results` is bounded to keep a single request from fanning out into an
/// unbounded (and unbudgeted) amount of provider work.
const MAX_RESULTS_LIMIT: u32 = 20;

/// Decide which engine serves a request, in precedence order:
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
///
/// With two or more engines configured and no explicit default, refuse rather
/// than guess: picking one would reintroduce exactly the silent-substitution
/// problem, and the error names the available engines so the fix is obvious.
fn resolve_engine(
    state: &AppState,
    requested: Option<&str>,
) -> Result<String, ApiError> {
    if let Some(engine) = requested.map(str::trim).filter(|e| !e.is_empty()) {
        return Ok(engine.to_string());
    }

    if let Some(configured) = state
        .settings
        .routing
        .default_search_engine
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        return Ok(configured.to_string());
    }

    let available = state.search_registry.configured_engines();
    match available.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(ApiError::InvalidRequest(
            "no search engine configured: add a [providers.<engine>] section \
             (supported: tavily, vertex) to config.toml"
                .to_string(),
        )),
        many => Err(ApiError::InvalidRequest(format!(
            "request omitted `engine` and multiple search engines are configured ({}); \
             send `engine` explicitly or set [routing] default_search_engine",
            many.join(", ")
        ))),
    }
}

pub async fn search(
    State(state): State<AppState>,
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
    search_inner(State(state), user, headers, Json(body))
        .instrument(span)
        .await
}

async fn search_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;
    tracing::Span::current().record("user_id", user.id);

    let attribution = crate::api::attribution::Attribution::extract(&body, &headers)?;

    let query = body["query"].as_str().unwrap_or("").to_string();
    if query.trim().is_empty() {
        return Err(ApiError::InvalidRequest(
            "query must not be empty".to_string(),
        ));
    }

    let max_results = match body.get("max_results").and_then(|v| v.as_u64()) {
        Some(n) if n == 0 => {
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
    let pseudo_model = format!("search/{}", engine);
    let policy_result = state
        .policy
        .check(&user, &pseudo_model)
        .await
        .map_err(|_| ApiError::Internal)?;
    match policy_result {
        PolicyDecision::Allow { .. } => {}
        PolicyDecision::Deny { reason, status, .. } => {
            return Err(ApiError::PolicyDenied { reason, status });
        }
    }

    // Pricing follows the images.rs precedent: `input_per_million` is reused as
    // a flat per-unit dollar rate (here: dollars per query), not a
    // per-million-token rate. See config.example.toml for the documented unit.
    let cost = state
        .settings
        .pricing
        .iter()
        .find(|p| p.model == pseudo_model)
        .map(|p| p.input_per_million)
        .unwrap_or(DEFAULT_COST_PER_QUERY);

    // ── Response cache ───────────────────────────────────────────────────────
    // Search queries are deterministic enough to cache by default; the key is
    // engine + query + options, and the TTL is shorter than for completions.
    let cache_key = if state.policy.cache_enabled(&user, &pseudo_model)
        && state.response_cache.search_eligible()
    {
        Some(crate::router::cache::search_cache_key(
            &engine,
            &query,
            max_results,
        ))
    } else {
        None
    };

    if let Some(ref key) = cache_key {
        if let Some(payload) = state.response_cache.get_search(key, &pseudo_model).await {
            tracing::info!(engine = engine.as_str(), "search cache hit");
            let results_returned = payload["results"].as_array().map(|r| r.len()).unwrap_or(0) as i64;
            record_search_cache_hit(
                &state,
                &user,
                &pseudo_model,
                &engine,
                results_returned,
                cost,
                &attribution,
            );
            let mut body = payload;
            body["usage"] = serde_json::json!({
                "results": results_returned,
                "cost_usd": 0.0,
                "cache_hit": true,
                "saved_usd": cost,
            });
            let mut response = Json(body).into_response();
            response.headers_mut().insert(
                crate::api::routes::completions::CACHE_HEADER,
                axum::http::HeaderValue::from_static("HIT"),
            );
            return Ok(response);
        }
    }

    let adapter = state
        .search_registry
        .get(&engine)
        .map_err(ApiError::ProviderError)?;

    let req = SearchRequest {
        query: query.clone(),
        max_results,
    };

    let start = Instant::now();
    let result = adapter
        .search(&req)
        .await
        .map_err(ApiError::ProviderError)?;
    let latency_ms = start.elapsed().as_millis() as i64;

    let results_returned = result.results.len() as i64;

    let span = tracing::Span::current();
    span.record("cost.usd", cost);
    span.record("results.count", results_returned);

    #[cfg(feature = "otel")]
    {
        crate::telemetry::metrics::record_request(&pseudo_model, &engine, "ok");
        crate::telemetry::metrics::record_cost(&pseudo_model, &engine, user.id, cost);
        crate::telemetry::metrics::record_duration(
            &pseudo_model,
            &engine,
            false,
            latency_ms as f64,
        );
    }

    #[cfg(feature = "prometheus")]
    if let Some(ref metrics) = state.app_metrics {
        metrics.record_request(&pseudo_model, &engine, "ok");
        metrics.record_cost(&pseudo_model, &engine, cost);
    }

    // Fire-and-forget cost recording
    let state_clone = state.clone();
    let engine_clone = engine.clone();
    let pseudo_model_clone = pseudo_model.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    let user_project = attribution.project_or(user.api_key_project.clone());
    let attr_correlation = attribution.correlation_id.clone();
    let attr_tags = attribution.tags_json();

    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model: pseudo_model_clone.clone(),
            routed_model: pseudo_model_clone.clone(),
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
            tags: "[]".to_string(),
            project: user_project.clone(),
            attribution_correlation_id: attr_correlation.clone(),
            attribution_tags: attr_tags.clone(),
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
                    model: pseudo_model_clone,
                    provider: engine_clone,
                    project: user_project.clone(),
                    tokens_in: results_returned,
                    tokens_out: 0,
                    cost_usd: cost,
                    api_key_id,
                    attribution_correlation_id: attr_correlation.clone(),
                    attribution_tags: attr_tags.clone(),
                };
                if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
                    tracing::error!("Failed to record search cost: {}", e);
                }
        }
    });

    let payload = serde_json::json!({
        "engine": result.engine,
        "results": result.results,
        "usage": {
            "results": results_returned,
            "cost_usd": cost,
        }
    });

    if let Some(key) = cache_key {
        state
            .response_cache
            .put_search(&key, &pseudo_model, payload.clone(), cost)
            .await;
    }

    let mut response = Json(payload).into_response();
    response.headers_mut().insert(
        crate::api::routes::completions::CACHE_HEADER,
        axum::http::HeaderValue::from_static("MISS"),
    );
    Ok(response)
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
        project: user.api_key_project.clone(),
        tokens_in: results_returned,
        tokens_out: 0,
        // Interpreted as the avoided cost by `create_cache_hit`.
        cost_usd: avoided_cost,
        api_key_id: user.api_key_id,
        attribution_correlation_id: attribution.correlation_id.clone(),
        attribution_tags: attribution.tags_json(),
    };
    tokio::spawn(async move {
        if let Err(e) = CostRepository::create_cache_hit(&*state.db, ledger).await {
            tracing::error!("Failed to record search cache-hit usage: {}", e);
        }
    });
}
