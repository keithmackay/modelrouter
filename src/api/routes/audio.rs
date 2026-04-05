use axum::{
    extract::{Multipart, State},
    http::header::CONTENT_TYPE,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;
use tracing::Instrument;

use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::models::{NewCostLedgerEntry, NewPrompt},
    router::policy::PolicyDecision,
};

pub async fn speech(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!(
        "speech",
        user_id = tracing::field::Empty,
        model = tracing::field::Empty,
    );
    speech_inner(State(state), user, Json(body))
        .instrument(span)
        .await
}

async fn speech_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;
    tracing::Span::current().record("user_id", user.id);

    let model = body["model"].as_str().unwrap_or("tts-1").to_string();
    tracing::Span::current().record("model", model.as_str());

    // Policy check
    let _concurrency_permit = match state
        .policy
        .check(&user, &model)
        .instrument(tracing::info_span!("modelrouter.policy_check"))
        .await
        .map_err(|_| ApiError::Internal)?
    {
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

    // Circuit breaker check
    if state.circuit_breaker.is_open(provider_name) {
        return Err(ApiError::ProviderError(anyhow::anyhow!(
            "{provider_name} is circuit-broken"
        )));
    }

    // Build OpenAI URL
    let base = provider_config
        .api_base
        .clone()
        .unwrap_or_else(|| "https://api.openai.com".to_string());
    let url = format!("{}/v1/audio/speech", base.trim_end_matches('/'));

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .bearer_auth(&provider_config.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            state.circuit_breaker.record_failure(provider_name);
            ApiError::ProviderError(anyhow::anyhow!("request failed: {e}"))
        })?;

    state.circuit_breaker.record_success(provider_name);

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let msg = resp
            .text()
            .await
            .unwrap_or_else(|_| "provider error".to_string());
        return Err(ApiError::ProviderError(anyhow::anyhow!(
            "provider returned {status}: {msg}"
        )));
    }

    // Capture content-type before consuming body
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("audio/mpeg")
        .to_string();

    let bytes = resp.bytes().await.map_err(|e| {
        ApiError::ProviderError(anyhow::anyhow!("failed to read response: {e}"))
    })?;

    // Cost logging
    let char_count = body["input"].as_str().map(|s| s.len()).unwrap_or(0);
    let cost = (char_count as f64 / 1000.0) * 0.015;
    let db = state.db.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    let model_clone = model.clone();
    let provider_clone = provider_name.clone();

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
            prompt_tokens: char_count as i64,
            completion_tokens: 0,
            cost_usd: cost,
            latency_ms: None,
            tags: "[]".to_string(),
            project: None,
        };
        match PromptRepository::create(&*db, prompt).await {
            Ok(saved_prompt) => {
                let ledger = NewCostLedgerEntry {
                    user_id,
                    prompt_id: saved_prompt.id,
                    model: model_clone,
                    provider: provider_clone,
                    project: None,
                    tokens_in: char_count as i64,
                    tokens_out: 0,
                    cost_usd: cost,
                    api_key_id,
                };
                if let Err(e) = CostRepository::create(&*db, ledger).await {
                    tracing::error!("Failed to record speech cost: {e}");
                }
            }
            Err(e) => tracing::error!("Failed to log speech prompt: {e}"),
        }
    });

    Ok(([(CONTENT_TYPE, content_type)], bytes).into_response())
}

pub async fn transcriptions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    multipart: Multipart,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!(
        "transcriptions",
        user_id = tracing::field::Empty,
        model = tracing::field::Empty,
    );
    transcriptions_inner(State(state), user, multipart)
        .instrument(span)
        .await
}

async fn transcriptions_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    mut multipart: Multipart,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;
    tracing::Span::current().record("user_id", user.id);

    let model = "whisper-1".to_string();
    tracing::Span::current().record("model", model.as_str());

    // Policy check
    let _concurrency_permit = match state
        .policy
        .check(&user, &model)
        .instrument(tracing::info_span!("modelrouter.policy_check"))
        .await
        .map_err(|_| ApiError::Internal)?
    {
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

    // Circuit breaker check
    if state.circuit_breaker.is_open(provider_name) {
        return Err(ApiError::ProviderError(anyhow::anyhow!(
            "{provider_name} is circuit-broken"
        )));
    }

    // Reassemble multipart form
    let mut form = reqwest::multipart::Form::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| ApiError::InvalidRequest("invalid multipart".to_string()))?
    {
        let name = field.name().unwrap_or("file").to_string();
        let filename = field.file_name().map(|s| s.to_string());
        let ct = field.content_type().map(|s| s.to_string());
        let bytes = field
            .bytes()
            .await
            .map_err(|_| ApiError::InvalidRequest("read error".to_string()))?;
        let raw = bytes.to_vec();
        let mut part = reqwest::multipart::Part::bytes(raw.clone());
        if let Some(f) = filename {
            part = part.file_name(f);
        }
        if let Some(c) = ct {
            part = match part.mime_str(&c) {
                Ok(p) => p,
                Err(_) => reqwest::multipart::Part::bytes(raw),
            };
        }
        form = form.part(name, part);
    }

    // Build OpenAI URL
    let base = provider_config
        .api_base
        .clone()
        .unwrap_or_else(|| "https://api.openai.com".to_string());
    let url = format!("{}/v1/audio/transcriptions", base.trim_end_matches('/'));

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .bearer_auth(&provider_config.api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| {
            state.circuit_breaker.record_failure(provider_name);
            ApiError::ProviderError(anyhow::anyhow!("request failed: {e}"))
        })?;

    state.circuit_breaker.record_success(provider_name);

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let msg = resp
            .text()
            .await
            .unwrap_or_else(|_| "provider error".to_string());
        return Err(ApiError::ProviderError(anyhow::anyhow!(
            "provider returned {status}: {msg}"
        )));
    }

    let result: Value = resp.json().await.map_err(|e| {
        ApiError::ProviderError(anyhow::anyhow!("failed to parse response: {e}"))
    })?;

    // Cost logging
    let cost = 0.006_f64;
    let db = state.db.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    let model_clone = model.clone();
    let provider_clone = provider_name.clone();

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
            prompt_tokens: 1,
            completion_tokens: 0,
            cost_usd: cost,
            latency_ms: None,
            tags: "[]".to_string(),
            project: None,
        };
        match PromptRepository::create(&*db, prompt).await {
            Ok(saved_prompt) => {
                let ledger = NewCostLedgerEntry {
                    user_id,
                    prompt_id: saved_prompt.id,
                    model: model_clone,
                    provider: provider_clone,
                    project: None,
                    tokens_in: 1,
                    tokens_out: 0,
                    cost_usd: cost,
                    api_key_id,
                };
                if let Err(e) = CostRepository::create(&*db, ledger).await {
                    tracing::error!("Failed to record transcription cost: {e}");
                }
            }
            Err(e) => tracing::error!("Failed to log transcription prompt: {e}"),
        }
    });

    Ok(Json(result).into_response())
}
