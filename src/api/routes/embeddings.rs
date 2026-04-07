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
    providers::embedding::EmbeddingRequest,
    router::policy::PolicyDecision,
};

pub async fn embeddings(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!(
        "embeddings",
        user_id = tracing::field::Empty,
        model = tracing::field::Empty,
        provider = tracing::field::Empty,
        "cost.usd" = tracing::field::Empty,
        "tokens.prompt" = tracing::field::Empty,
    );
    embeddings_inner(State(state), user, Json(body))
        .instrument(span)
        .await
}

async fn embeddings_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;
    let model = body["model"]
        .as_str()
        .unwrap_or("text-embedding-3-small")
        .to_string();

    // Policy check
    let policy_result = state
        .policy
        .check(&user, &model)
        .await
        .map_err(|_| ApiError::Internal)?;
    match policy_result {
        PolicyDecision::Allow { .. } => {}
        PolicyDecision::Deny { reason, status, .. } => {
            return Err(ApiError::PolicyDenied { reason, status });
        }
    }

    // Parse input — accepts either a single string or an array of strings
    let input: Vec<String> = match &body["input"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => {
            return Err(ApiError::InvalidRequest(
                "input must be a string or array of strings".to_string(),
            ))
        }
    };

    if input.is_empty() {
        return Err(ApiError::InvalidRequest("input must not be empty".to_string()));
    }

    let (provider_name, canonical_model) = state.router.resolve(&model);
    let adapter = state
        .embedding_registry
        .get(&provider_name)
        .map_err(ApiError::ProviderError)?;

    let span = tracing::Span::current();
    span.record("user_id", user.id);
    span.record("model", canonical_model.as_str());
    span.record("provider", provider_name.as_str());

    let req = EmbeddingRequest {
        model: canonical_model.clone(),
        input,
    };

    let start = Instant::now();
    let result = adapter.embed(&req).await.map_err(ApiError::ProviderError)?;
    let latency_ms = start.elapsed().as_millis() as i64;

    let cost = state
        .cost_calc
        .calculate(&canonical_model, result.prompt_tokens, 0);

    span.record("cost.usd", cost);
    span.record("tokens.prompt", result.prompt_tokens as u64);

    #[cfg(feature = "otel")]
    {
        crate::telemetry::metrics::record_request(&canonical_model, &provider_name, "ok");
        crate::telemetry::metrics::record_tokens(
            &canonical_model, &provider_name,
            result.prompt_tokens, 0,
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
        metrics.record_request(&canonical_model, &provider_name, "ok");
        metrics.record_tokens(&canonical_model, &provider_name, result.prompt_tokens, 0);
        metrics.record_cost(&canonical_model, &provider_name, cost);
    }

    // Fire-and-forget cost recording
    let state_clone = state.clone();
    let model_clone = model.clone();
    let canonical_clone = canonical_model.clone();
    let provider_clone = provider_name.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    let user_project = user.api_key_project.clone();
    let prompt_tokens = result.prompt_tokens;

    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model: model_clone,
            routed_model: canonical_clone.clone(),
            provider: provider_clone.clone(),
            messages: "[]".to_string(), // embeddings have no chat messages
            response: None,
            finish_reason: None,
            prompt_tokens: prompt_tokens as i64,
            completion_tokens: 0,
            cost_usd: cost,
            latency_ms: Some(latency_ms),
            tags: "[]".to_string(),
            project: user_project.clone(),
        };
        match PromptRepository::create(&*state_clone.db, prompt).await {
            Ok(saved) => {
                let ledger = NewCostLedgerEntry {
                    user_id,
                    prompt_id: saved.id,
                    model: canonical_clone,
                    provider: provider_clone,
                    project: user_project.clone(),
                    tokens_in: prompt_tokens as i64,
                    tokens_out: 0,
                    cost_usd: cost,
                    api_key_id,
                };
                if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
                    tracing::error!("Failed to record embedding cost: {}", e);
                }
            }
            Err(e) => tracing::error!("Failed to record embedding prompt: {}", e),
        }
    });

    // Build OpenAI-compatible response
    let data: Vec<Value> = result
        .embeddings
        .iter()
        .enumerate()
        .map(|(i, emb)| {
            serde_json::json!({
                "object": "embedding",
                "index": i,
                "embedding": emb,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "object": "list",
        "data": data,
        "model": canonical_model,
        "usage": {
            "prompt_tokens": result.prompt_tokens,
            "total_tokens": result.prompt_tokens,
        }
    }))
    .into_response())
}
