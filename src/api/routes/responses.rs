use std::time::Instant;

use axum::{extract::State, response::{IntoResponse, Response}, Json};
use serde_json::Value;
use tracing::Instrument;

use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::models::{NewCostLedgerEntry, NewPrompt},
    providers::adapter::NormalizedRequest,
    router::policy::PolicyDecision,
};

pub async fn responses_handler(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!(
        "responses_handler",
        user_id = tracing::field::Empty,
        model = tracing::field::Empty,
    );
    responses_inner(State(state), user, Json(body))
        .instrument(span)
        .await
}

async fn responses_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;
    tracing::Span::current().record("user_id", user.id);

    let model = body["model"]
        .as_str()
        .unwrap_or(&state.settings.routing.default_model)
        .to_string();

    tracing::Span::current().record("model", model.as_str());

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
        PolicyDecision::Deny { reason, status, .. } => {
            return Err(ApiError::PolicyDenied { reason, status });
        }
    };

    // Route the model
    let (provider_name, canonical_model) = state.router.resolve(&model);

    // Translate body: if messages absent and input is a string, synthesize messages
    let mut body = body;
    let has_messages = body["messages"].is_array();
    if !has_messages {
        if let Some(input_str) = body["input"].as_str() {
            let messages = serde_json::json!([{"role": "user", "content": input_str}]);
            body["messages"] = messages;
        } else if body["input"].is_array() {
            // Array-form input: pass through as messages directly
            body["messages"] = body["input"].clone();
            body.as_object_mut().map(|m| m.remove("input"));
        }
    }
    // Remove "input" key
    if let Some(obj) = body.as_object_mut() {
        obj.remove("input");
    }

    let norm_req = NormalizedRequest {
        model: canonical_model.clone(),
        messages: body["messages"].as_array().cloned().unwrap_or_default(),
        stream: false,
        temperature: body["temperature"].as_f64(),
        max_tokens: body["max_tokens"].as_u64().map(|v| v as u32),
        extra_params: serde_json::Value::Object(Default::default()),
    };

    let start = Instant::now();

    // Check circuit breaker before calling provider
    if state.circuit_breaker.is_open(&provider_name) {
        return Err(ApiError::ProviderError(anyhow::anyhow!("{provider_name} is circuit-broken")));
    }

    let adapter = state
        .provider_registry
        .get(&provider_name)
        .map_err(ApiError::ProviderError)?;
    let result = adapter.complete(&norm_req).await.map_err(|e| {
        state.circuit_breaker.record_failure(&provider_name);
        ApiError::ProviderError(e)
    })?;
    state.circuit_breaker.record_success(&provider_name);

    let latency_ms = start.elapsed().as_millis() as i64;

    let cost = state
        .cost_calc
        .calculate(&canonical_model, result.prompt_tokens, result.completion_tokens);

    // Fire-and-forget cost logging
    let db = state.db.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    let model_clone = model.clone();
    let canonical_clone = canonical_model.clone();
    let provider_clone = provider_name.clone();
    let messages_json = serde_json::to_string(
        &body["messages"].as_array().cloned().unwrap_or_default(),
    )
    .unwrap_or_default();
    let response_clone = result.content.clone();
    let finish_clone = result.finish_reason.clone();
    let prompt_tokens = result.prompt_tokens;
    let completion_tokens = result.completion_tokens;

    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model: model_clone.clone(),
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
        match PromptRepository::create(&*db, prompt).await {
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
                    api_key_id,
                };
                let _ = CostRepository::create(&*db, ledger).await;
            }
            Err(e) => tracing::error!("Failed to record responses prompt: {e}"),
        }
    });

    let response_body = serde_json::json!({
        "id": format!("resp_{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()),
        "object": "response",
        "model": canonical_model,
        "choices": [{
            "message": {
                "role": "assistant",
                "content": result.content
            },
            "finish_reason": result.finish_reason
        }],
        "usage": {
            "input_tokens": prompt_tokens,
            "output_tokens": completion_tokens
        }
    });

    Ok(Json(response_body).into_response())
}
