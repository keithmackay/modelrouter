use axum::{extract::State, response::{IntoResponse, Response}, Json};
use serde_json::Value;
use crate::api::{app::AppState, auth::AuthenticatedUser, error::ApiError};
use crate::router::policy::PolicyDecision;
use crate::db::models::{NewCostLedgerEntry, NewPrompt};

pub async fn image_generations(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;

    let model = body["model"]
        .as_str()
        .unwrap_or("dall-e-3")
        .to_string();
    let quality = body["quality"]
        .as_str()
        .unwrap_or("standard")
        .to_string();
    let n_images = body["n"].as_u64().unwrap_or(1) as i64;

    // Policy check
    match state.policy.check(&user, &model).await.map_err(|_| ApiError::Internal)? {
        PolicyDecision::Deny { reason, status, .. } => {
            return Err(ApiError::PolicyDenied { reason, status });
        }
        PolicyDecision::Allow { .. } => {}
    }

    // Get provider config
    let provider_name = &state.settings.routing.default_provider;
    let provider_config = state
        .settings
        .providers
        .get(provider_name)
        .cloned()
        .unwrap_or_else(|| crate::config::schema::ProviderConfig {
            api_key: String::new(),
            api_base: None,
            timeout_secs: 60,
            api_version: None,
            region: None,
        });

    // Check circuit breaker before calling provider
    if state.circuit_breaker.is_open(provider_name) {
        return Err(ApiError::ProviderError(anyhow::anyhow!("{provider_name} is circuit-broken")));
    }

    // Call image adapter
    let adapter = crate::providers::openai_images::OpenAIImageAdapter::new(&provider_config);
    let result = adapter.generate_image(&body).await.map_err(|e| {
        state.circuit_breaker.record_failure(provider_name);
        ApiError::ProviderError(e)
    })?;
    state.circuit_breaker.record_success(provider_name);

    // Calculate cost
    let pricing_key = format!("{}/{}", model, quality);
    let cost_per_image = state
        .settings
        .pricing
        .iter()
        .find(|p| p.model == pricing_key)
        .map(|p| p.input_per_million)
        .unwrap_or_else(|| {
            // Hard-coded defaults
            match pricing_key.as_str() {
                "dall-e-3/hd" => 0.080,
                "dall-e-3/standard" => 0.040,
                _ => 0.020,
            }
        });
    let cost = cost_per_image * n_images as f64;

    // Fire-and-forget cost logging
    let state_clone = state.clone();
    let model_clone = model.clone();
    let provider_clone = provider_name.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;

    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model: model_clone.clone(),
            routed_model: model_clone.clone(),
            provider: provider_clone.clone(),
            messages: "[]".to_string(),
            response: None,
            finish_reason: None,
            prompt_tokens: n_images,
            completion_tokens: 0,
            cost_usd: cost,
            latency_ms: None,
            tags: "[]".to_string(),
            project: None,
        };
        match PromptRepository::create(&*state_clone.db, prompt).await {
            Ok(saved_prompt) => {
                let ledger = NewCostLedgerEntry {
                    user_id,
                    prompt_id: saved_prompt.id,
                    model: model_clone.clone(),
                    provider: provider_clone.clone(),
                    project: None,
                    tokens_in: n_images,
                    tokens_out: 0,
                    cost_usd: cost,
                    api_key_id,
                };
                if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
                    tracing::error!("Failed to record image cost: {}", e);
                }
            }
            Err(e) => {
                tracing::error!("Failed to log image prompt: {}", e);
            }
        }
    });

    Ok(Json(result).into_response())
}
