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

pub async fn search(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!(
        "search",
        user_id = tracing::field::Empty,
        engine = tracing::field::Empty,
        "cost.usd" = tracing::field::Empty,
        "results.count" = tracing::field::Empty,
    );
    search_inner(State(state), user, Json(body))
        .instrument(span)
        .await
}

async fn search_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;
    tracing::Span::current().record("user_id", user.id);

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

    let engine = body["engine"].as_str().unwrap_or("tavily").to_string();
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

    // Pricing follows the images.rs precedent: `input_per_million` is reused
    // as a flat per-unit dollar rate (here: dollars per query), not a
    // per-million-token rate. See config.example.toml for the documented unit.
    let cost = state
        .settings
        .pricing
        .iter()
        .find(|p| p.model == pseudo_model)
        .map(|p| p.input_per_million)
        .unwrap_or(DEFAULT_COST_PER_QUERY);

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
    let user_project = user.api_key_project.clone();

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
        };
        match PromptRepository::create(&*state_clone.db, prompt).await {
            Ok(saved) => {
                let ledger = NewCostLedgerEntry {
                    user_id,
                    prompt_id: Some(saved.id),
                    model: pseudo_model_clone,
                    provider: engine_clone,
                    project: user_project.clone(),
                    tokens_in: results_returned,
                    tokens_out: 0,
                    cost_usd: cost,
                    api_key_id,
                };
                if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
                    tracing::error!("Failed to record search cost: {}", e);
                }
            }
            Err(e) => tracing::error!("Failed to record search prompt: {}", e),
        }
    });

    Ok(Json(serde_json::json!({
        "engine": result.engine,
        "results": result.results,
        "usage": {
            "results": results_returned,
            "cost_usd": cost,
        }
    }))
    .into_response())
}
