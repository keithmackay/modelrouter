//! Capture for requests that failed.
//!
//! The success path writes a `prompts` row. Until this module existed there was
//! no counterpart, so a request the router rejected — or that a provider
//! rejected — left no trace in the router at all, and diagnosis fell back to
//! reading the calling application's logs.
//!
//! This is deliberately a **single choke point** wrapped around each route's
//! whole handler, not a call sprinkled at every `return Err(...)`. There are a
//! dozen-odd error returns in the completions path alone; capture-by-discipline
//! means the next one added silently escapes, which is precisely the class of
//! gap this module exists to close. Wrapping the handler makes capture
//! structural: if the handler returns `Err`, it is recorded.
//!
//! Recording is best-effort and never changes the response: a failure to log a
//! failure must not turn a 502 into a 500.

use crate::api::{app::AppState, error::ApiError};
use crate::db::models::{FailureStage, NewRequestFailure};
use crate::db::repositories::failures::FailureRepository;

/// What the router knew about the request before it failed.
pub struct FailureContext {
    pub endpoint: &'static str,
    pub request_model: String,
    pub routed_model: Option<String>,
    pub provider: Option<String>,
    pub user_id: Option<i64>,
    pub api_key_id: Option<i64>,
    pub project: Option<String>,
    pub attribution_correlation_id: Option<String>,
    pub attribution_tags: String,
    pub latency_ms: Option<i64>,
}

/// Build the capture context from the request as it arrived.
///
/// Resolution is repeated here (cheaply, and without side effects) rather than
/// threaded out of the handler: the handler may fail BEFORE it resolves, and a
/// failure record that cannot say which provider the model would have gone to
/// is most of the way to useless — that missing field is exactly what made the
/// "Unknown provider: anthropic" run so slow to diagnose.
pub fn context_from_request(
    endpoint: &'static str,
    state: &AppState,
    user: &crate::db::models::User,
    headers: &axum::http::HeaderMap,
    body: &serde_json::Value,
) -> FailureContext {
    let request_model = body["model"]
        .as_str()
        .unwrap_or(&state.settings.routing.default_model)
        .to_string();
    let (provider, routed_model) = {
        let (p, m) = state.router.resolve(&request_model);
        (Some(p), Some(m))
    };
    // Attribution is best-effort here: an unparseable attribution block is
    // itself a failure worth recording, and must not prevent the record.
    let attribution = crate::api::attribution::Attribution::extract(body, headers).ok();

    FailureContext {
        endpoint,
        request_model,
        routed_model,
        provider,
        user_id: Some(user.id),
        api_key_id: user.api_key_id,
        project: user.api_key_project.clone(),
        attribution_correlation_id: attribution
            .as_ref()
            .and_then(|a| a.correlation_id.clone()),
        attribution_tags: attribution
            .as_ref()
            .map(|a| a.tags_json())
            .unwrap_or_else(|| "{}".to_string()),
        latency_ms: None,
    }
}

/// Classify WHERE the request died from the error the router produced.
///
/// The provider/resolve split matters more than it looks: a resolve failure is
/// always the operator's to fix (a provider is missing from config, or the
/// caller asked for one that was never configured) whereas a provider failure
/// is usually the upstream's. Lumping them together is what let 196
/// "Unknown provider: anthropic" errors read as ordinary upstream flakiness.
fn stage_for(err: &ApiError) -> FailureStage {
    match err {
        ApiError::PolicyDenied { .. } | ApiError::Unauthorized | ApiError::Forbidden => {
            FailureStage::Policy
        }
        ApiError::InvalidRequest(_) => FailureStage::Request,
        ApiError::ProviderError(e) => {
            let msg = e.to_string().to_lowercase();
            if msg.contains("unknown provider")
                || msg.contains("no embedding adapter for provider")
                || msg.contains("no adapter for provider")
            {
                FailureStage::Resolve
            } else {
                FailureStage::Provider
            }
        }
        ApiError::Internal => FailureStage::Internal,
    }
}

fn status_for(err: &ApiError) -> i64 {
    match err {
        ApiError::Unauthorized => 401,
        ApiError::Forbidden => 403,
        ApiError::ProviderError(_) => 502,
        ApiError::InvalidRequest(_) => 400,
        ApiError::PolicyDenied { status, .. } => *status as i64,
        ApiError::Internal => 500,
    }
}

/// Record a failed request. Best-effort: logs and moves on if the write fails.
pub async fn record_failure(state: &AppState, ctx: FailureContext, err: &ApiError) {
    let stage = stage_for(err);
    let message = err.to_string();

    // Always emit a log line too, so a router without a reachable DB still
    // surfaces the failure somewhere.
    tracing::warn!(
        endpoint = ctx.endpoint,
        request_model = ctx.request_model.as_str(),
        routed_model = ctx.routed_model.as_deref().unwrap_or("-"),
        provider = ctx.provider.as_deref().unwrap_or("-"),
        stage = stage.as_str(),
        error = %message,
        "request failed"
    );

    let record = NewRequestFailure {
        user_id: ctx.user_id,
        api_key_id: ctx.api_key_id,
        endpoint: ctx.endpoint.to_string(),
        request_model: ctx.request_model,
        routed_model: ctx.routed_model,
        provider: ctx.provider,
        stage,
        status_code: Some(status_for(err)),
        error_message: message,
        attempts: 1,
        latency_ms: ctx.latency_ms,
        project: ctx.project,
        attribution_correlation_id: ctx.attribution_correlation_id,
        attribution_tags: ctx.attribution_tags,
    };

    if let Err(e) = FailureRepository::create(&*state.db, record).await {
        tracing::error!(error = %e, "failed to persist request failure");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_provider_is_a_resolve_failure_not_a_provider_failure() {
        let err = ApiError::ProviderError(anyhow::anyhow!("Unknown provider: anthropic"));
        assert_eq!(stage_for(&err), FailureStage::Resolve);
    }

    #[test]
    fn missing_embedding_adapter_is_a_resolve_failure() {
        let err = ApiError::ProviderError(anyhow::anyhow!(
            "No embedding adapter for provider: ollama"
        ));
        assert_eq!(stage_for(&err), FailureStage::Resolve);
    }

    #[test]
    fn a_genuine_upstream_error_stays_a_provider_failure() {
        let err = ApiError::ProviderError(anyhow::anyhow!("upstream returned 500"));
        assert_eq!(stage_for(&err), FailureStage::Provider);
    }

    #[test]
    fn budget_denial_is_a_policy_failure_and_keeps_its_status() {
        let err = ApiError::PolicyDenied {
            reason: "monthly budget exceeded".into(),
            status: 429,
        };
        assert_eq!(stage_for(&err), FailureStage::Policy);
        assert_eq!(status_for(&err), 429);
    }
}
