//! `POST /v1/systemone` — JSON passthrough to TypeSafe System One.
//!
//! System One answers batched typed questions (Choice / Score / Noul) about a
//! `state` payload. Callers send the TypeSafe request body unchanged; the router
//! adds the TypeSafe key from `[providers.typesafe]`, so no caller ever holds it.
//!
//! The model is gated as the pseudo-model `systemone/{model}` (e.g.
//! `systemone/jev-latest`) so allow-lists and budgets apply exactly as they do
//! to `search/{engine}`. Cost comes from a `[[pricing]]` entry of that name,
//! priced per million tokens off the response's `usage` block.
//!
//! Upstream statuses pass through unchanged — 429 and 529 in particular, because
//! TypeSafe's contract is that the client backs off on them — with one
//! exception: a 401/403 from TypeSafe means the ROUTER's key is wrong, and
//! handing the caller a 401 would tell it that ITS key is wrong. Those become
//! 502 with a message naming the real cause.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use axum::{
    extract::State,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;
use tracing::Instrument;

use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    config::schema::PricingEntry,
    db::models::{NewCostLedgerEntry, NewPrompt},
    router::policy::PolicyDecision,
};

/// Provider section holding the TypeSafe key: `[providers.typesafe]`.
pub const PROVIDER_NAME: &str = "typesafe";

/// Default System One base; the route appends `/systemone`.
const DEFAULT_API_BASE: &str = "https://api.typesafe.ai/v1";

/// One pooled client for every call; the per-provider timeout is applied per
/// request so a config hot-reload changes it without rebuilding the pool.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .build()
            .expect("Failed to build reqwest client")
    })
}

/// Cost of one call from the response's `usage` block. An unpriced model costs
/// zero but is still metered (tokens recorded), and says so at WARN so the gap
/// is visible instead of silently free.
pub fn systemone_cost(
    pricing: &[PricingEntry],
    pseudo_model: &str,
    input_tokens: i64,
    output_tokens: i64,
) -> f64 {
    match pricing.iter().find(|p| p.model == pseudo_model) {
        Some(p) => {
            (input_tokens as f64 * p.input_per_million
                + output_tokens as f64 * p.output_per_million)
                / 1_000_000.0
        }
        None => {
            tracing::warn!(
                model = pseudo_model,
                "no [[pricing]] entry for System One model; recording cost as $0"
            );
            0.0
        }
    }
}

pub async fn systemone(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!(
        "systemone",
        user_id = tracing::field::Empty,
        model = tracing::field::Empty,
        "cost.usd" = tracing::field::Empty,
    );
    systemone_inner(state, user, headers, body)
        .instrument(span)
        .await
}

async fn systemone_inner(
    state: AppState,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    body: Value,
) -> Result<Response, ApiError> {
    crate::api::routes::reject_experiment_header("/v1/systemone", &headers)?;
    let user = user.0;
    tracing::Span::current().record("user_id", user.id);

    let attribution = crate::api::attribution::Attribution::extract(&body, &headers)?;
    // The body is otherwise forwarded to TypeSafe verbatim; the attribution
    // extension field is router-internal bookkeeping (already captured above)
    // and must not leak to the third-party upstream.
    let mut body = body;
    if let Some(obj) = body.as_object_mut() {
        obj.remove(crate::api::attribution::BODY_FIELD);
    }

    let model = body["model"]
        .as_str()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| ApiError::InvalidRequest("model must be a non-empty string".to_string()))?
        .to_string();
    if !body["questions"].is_object() {
        return Err(ApiError::InvalidRequest(
            "questions must be an object keyed by question id".to_string(),
        ));
    }
    tracing::Span::current().record("model", model.as_str());

    let pseudo_model = format!("systemone/{}", model);
    let _concurrency_permit = match state
        .policy
        .check(&user, &pseudo_model)
        .await
        .map_err(|_| ApiError::Internal)?
    {
        PolicyDecision::Allow { max_concurrent } => match max_concurrent {
            Some(max) => match state.concurrency.try_acquire(user.id, max) {
                Some(permit) => Some(permit),
                None => {
                    return Err(ApiError::PolicyDenied {
                        reason: "concurrent request limit exceeded".to_string(),
                        status: 429,
                    });
                }
            },
            None => None,
        },
        PolicyDecision::Deny { reason, status, .. } => {
            return Err(ApiError::PolicyDenied { reason, status });
        }
    };

    let provider = state.settings.providers.get(PROVIDER_NAME).ok_or_else(|| {
        ApiError::ProviderError(anyhow::anyhow!(
            "System One is not configured: add a [providers.{}] section with api_key",
            PROVIDER_NAME
        ))
    })?;
    if provider.api_key.trim().is_empty() {
        return Err(ApiError::ProviderError(anyhow::anyhow!(
            "[providers.{}] has no api_key (set it, or MODELROUTER_PROVIDERS__TYPESAFE__API_KEY)",
            PROVIDER_NAME
        )));
    }
    let url = format!(
        "{}/systemone",
        provider
            .api_base
            .as_deref()
            .unwrap_or(DEFAULT_API_BASE)
            .trim_end_matches('/')
    );

    let start = Instant::now();
    let upstream = http_client()
        .post(&url)
        .bearer_auth(&provider.api_key)
        .timeout(Duration::from_secs(provider.timeout_secs))
        .json(&body)
        .send()
        .await
        .map_err(|e| ApiError::ProviderError(anyhow::anyhow!("System One request failed: {}", e)))?;
    let latency_ms = start.elapsed().as_millis() as i64;

    let status = upstream.status();
    let content_type = upstream.headers().get(axum::http::header::CONTENT_TYPE).cloned();
    let bytes = upstream
        .bytes()
        .await
        .map_err(|e| ApiError::ProviderError(anyhow::anyhow!("System One response unreadable: {}", e)))?;

    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(ApiError::ProviderError(anyhow::anyhow!(
            "TypeSafe rejected the router's [providers.{}] api_key (HTTP {})",
            PROVIDER_NAME,
            status.as_u16()
        )));
    }

    if !status.is_success() {
        tracing::warn!(status = status.as_u16(), "System One returned an error; passing it through");
        return Ok(passthrough(status, content_type, bytes));
    }

    let parsed: Value = serde_json::from_slice(&bytes).map_err(|e| {
        ApiError::ProviderError(anyhow::anyhow!("System One returned non-JSON success body: {}", e))
    })?;
    let input_tokens = parsed["usage"]["input_tokens"].as_i64().unwrap_or(0);
    let output_tokens = parsed["usage"]["output_tokens"].as_i64().unwrap_or(0);
    let cost = systemone_cost(&state.settings.pricing, &pseudo_model, input_tokens, output_tokens);
    tracing::Span::current().record("cost.usd", cost);

    record_usage(
        &state,
        &user,
        &pseudo_model,
        &attribution,
        input_tokens,
        output_tokens,
        cost,
        latency_ms,
    );

    Ok(passthrough(status, content_type, bytes))
}

fn passthrough(
    status: reqwest::StatusCode,
    content_type: Option<HeaderValue>,
    bytes: bytes::Bytes,
) -> Response {
    let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = (status, bytes).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        content_type.unwrap_or_else(|| HeaderValue::from_static("application/json")),
    );
    response
}

/// Fire-and-forget prompt + cost rows, same storage policy as `/v1/search`.
/// The `state` payload is not logged: it is caller data, and the question
/// bodies alone say nothing useful without it.
#[allow(clippy::too_many_arguments)]
fn record_usage(
    state: &AppState,
    user: &crate::db::models::User,
    pseudo_model: &str,
    attribution: &crate::api::attribution::Attribution,
    input_tokens: i64,
    output_tokens: i64,
    cost: f64,
    latency_ms: i64,
) {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let state = state.clone();
    let pseudo_model = pseudo_model.to_string();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    let project = attribution.project_or(user.api_key_project.clone());
    let correlation_id = attribution.correlation_id.clone();
    let tags = attribution.tags_json();

    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model: pseudo_model.clone(),
            routed_model: pseudo_model.clone(),
            provider: PROVIDER_NAME.to_string(),
            messages: "[]".to_string(),
            response: None,
            finish_reason: None,
            prompt_tokens: input_tokens,
            completion_tokens: output_tokens,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: cost,
            latency_ms: Some(latency_ms),
            ttft_ms: None,
            attempts: None,
            tags: "[]".to_string(),
            project: project.clone(),
            attribution_correlation_id: correlation_id.clone(),
            attribution_tags: tags.clone(),
            experiment_id: None,
            experiment_variant: None,
        };
        let stored = match crate::db::prompt_store::apply_storage_policy(&state.storage.load(), prompt) {
            Some(p) => match PromptRepository::create(&*state.prompt_db, p).await {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::error!("Failed to record System One prompt: {}", e);
                    None
                }
            },
            None => None,
        };
        let ledger = NewCostLedgerEntry {
            user_id,
            prompt_id: stored.as_ref().map(|s| s.id),
            model: pseudo_model,
            provider: PROVIDER_NAME.to_string(),
            project,
            tokens_in: input_tokens,
            tokens_out: output_tokens,
            cost_usd: cost,
            api_key_id,
            attribution_correlation_id: correlation_id,
            attribution_tags: tags,
            experiment_id: None,
            experiment_variant: None,
            tokens_estimated: false,
        };
        if let Err(e) = CostRepository::create(&*state.db, ledger).await {
            tracing::error!("Failed to record System One cost: {}", e);
        }
    });
}
