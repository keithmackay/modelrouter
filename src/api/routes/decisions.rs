//! `POST /v1/decisions`: typed questions over a document, answered by a
//! decision-capable deployment, passed through the router.
//!
//! The router does what it does for every priced call and nothing else: it
//! authenticates the caller, applies the policy, resolves `model` through the
//! alias and scoped-alias maps to a `provider/deployment`, applies the tier
//! timeout ceiling, prices the answer from the provider's `usage`, writes the
//! prompt and cost-ledger rows, and returns the provider's body as it arrived
//! with `usage.cost_usd` and `x_router` added. See `providers::decision`.
//!
//! No retry and no fallback: a decision names one deployment, and a provider's
//! 4xx is the provider refusing this request, returned to the caller with the
//! provider's status and body.

use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde_json::Value;
use tracing::Instrument;

use super::router_meta::{CallCost, RouterMeta, TimingMeta, TokenMeta};

use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::models::{NewCostLedgerEntry, NewPrompt},
    providers::decision::{DecisionRequest, DecisionRequestInvalid, UpstreamRejected},
    providers::decision_registry::DecisionRegistry,
    router::engine::RequestRouter,
    router::policy::PolicyDecision,
};

const ENDPOINT: &str = "/v1/decisions";

pub async fn decisions(
    State(state): State<AppState>,
    Extension(registry): Extension<Arc<DecisionRegistry>>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!(
        "decisions",
        user_id = tracing::field::Empty,
        model = tracing::field::Empty,
        provider = tracing::field::Empty,
        "cost.usd" = tracing::field::Empty,
        "tokens.prompt" = tracing::field::Empty,
        "tokens.completion" = tracing::field::Empty,
    );
    // See api::failure_log: capture wraps the whole handler so no error return
    // can escape being recorded.
    let ctx = crate::api::failure_log::context_from_request(ENDPOINT, &state, &user.0, &headers, &body);
    let started = Instant::now();

    let result = decisions_inner(State(state.clone()), registry, user, headers, Json(body))
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

/// Resolve a decision request's `model` to `(provider, deployment)` without
/// ever substituting another model.
///
/// Chat resolution falls through to `routing.default_model` when a name is no
/// alias and carries no `provider/` prefix, and `strict_model_resolution`
/// decides whether that swap is refused or merely logged. Neither applies to a
/// decision: the caller named the one deployment that is to answer, and a
/// decision-only deployment is usually absent from the chat catalog, so the
/// swap would send the document to a chat model that cannot answer and tell
/// the caller it got what it asked for. So a name the alias maps do not know
/// resolves through the providers' `decision_deployments`, and a name no
/// provider lists is a 400 naming it, before anything is sent, whatever
/// `strict_model_resolution` says.
///
/// `requested_model` is what the caller sent; `model` is the same after the
/// scoped-alias step, and is what is resolved.
pub fn resolve_decision_model(
    router: &RequestRouter,
    registry: &DecisionRegistry,
    requested_model: &str,
    model: &str,
) -> Result<(String, String), ApiError> {
    let resolution = router.resolve_detailed(model);
    if !resolution.substituted {
        return Ok((resolution.provider, resolution.model));
    }
    if let Some(provider) = registry.provider_for_deployment(model) {
        tracing::debug!(
            requested_model,
            provider = provider.as_str(),
            "decisions: bare model resolved through the provider's decision_deployments"
        );
        return Ok((provider, model.trim().to_string()));
    }
    let scoped = if model != requested_model { format!(" (scoped alias '{model}')") } else { String::new() };
    tracing::warn!(
        requested_model,
        scoped_model = model,
        would_have_substituted = %format!("{}/{}", resolution.provider, resolution.model),
        "decisions: model does not resolve and no provider lists it as a decision deployment; refusing rather than substituting"
    );
    Err(ApiError::InvalidRequest(format!(
        "model '{requested_model}'{scoped} is not a configured alias, names no provider, and is not one of any \
         provider's decision_deployments; a decision is never answered by a substitute model \
         (routing.default_model and routing.strict_model_resolution do not apply to /v1/decisions). \
         Add the deployment to [providers.<provider>] decision_deployments, alias it in \
         [routing.model_aliases], or request it as 'provider/deployment'."
    )))
}

/// The body the caller receives for a provider's 4xx: the router's stable
/// code, the provider's status, and the provider's body under `upstream`.
pub fn upstream_rejected_body(rejected: &UpstreamRejected) -> Value {
    serde_json::json!({
        "error": {
            "message": format!("the provider rejected the decision request with HTTP {}", rejected.status),
            "type": "upstream_rejected",
            "code": "upstream_rejected",
            "upstream_status": rejected.status,
            "upstream": rejected.body,
        }
    })
}

/// Map a failed provider call: a typed 4xx passes through with its status, a
/// pre-flight refusal is the caller's 400, anything else is a provider error.
fn map_decision_error(err: anyhow::Error) -> ApiError {
    if let Some(rejected) = UpstreamRejected::find(&err) {
        return ApiError::Refused {
            status: rejected.status,
            body: upstream_rejected_body(rejected),
            reason: rejected.to_string(),
        };
    }
    if let Some(invalid) = DecisionRequestInvalid::find(&err) {
        return ApiError::InvalidRequest(invalid.0.clone());
    }
    ApiError::ProviderError(err)
}

async fn decisions_inner(
    State(state): State<AppState>,
    registry: Arc<DecisionRegistry>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    // Request receipt: `x_router.timing.total_ms` is measured from here.
    let received = Instant::now();
    super::reject_experiment_header(ENDPOINT, &headers)?;
    let user = user.0;
    let attribution = crate::api::attribution::Attribution::extract(&body, &headers)?;
    let attr_correlation = attribution.correlation_id.clone();
    let attr_tags = attribution.tags_json();

    if !body.is_object() {
        return Err(ApiError::InvalidRequest("the request body must be a JSON object".to_string()));
    }
    // No default model here, unlike embeddings: a decision deployment is not
    // interchangeable, and a caller that did not name one gets told so.
    let requested_model = body["model"]
        .as_str()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| ApiError::InvalidRequest("`model` is required: the decision deployment, or an alias for one".to_string()))?
        .to_string();
    // A scoped alias override for the request's attribution tags, as on chat.
    let model = state
        .router
        .scoped_name(&attribution.tags, &requested_model)
        .unwrap_or_else(|| requested_model.clone());

    let policy_result = state.policy.check(&user, &model).await.map_err(|_| ApiError::Internal)?;
    match policy_result {
        PolicyDecision::Allow { .. } => {}
        PolicyDecision::Deny { reason, status, .. } => {
            return Err(ApiError::PolicyDenied { reason, status });
        }
    }

    // A decision names one deployment, so `model` is never substituted: an
    // alias or `provider/deployment` resolves as on chat, a bare name resolves
    // through the providers' `decision_deployments`, and anything else is the
    // caller's 400 before anything is sent (`routing.default_model` and
    // `strict_model_resolution` play no part here). Then the operator disable
    // gate (issue #5).
    let (provider_name, canonical_model) = resolve_decision_model(&state.router, &registry, &requested_model, &model)?;
    state.router.check_available(&provider_name, &canonical_model)?;

    // A model that resolves to a provider with no decisions surface (or an
    // unconfigured one) is the caller's error to read, not a gateway fault.
    let adapter = registry.get(&provider_name).map_err(|e| {
        ApiError::InvalidRequest(format!(
            "model '{requested_model}' resolves to {provider_name}/{canonical_model}, which cannot answer a decision: {e}"
        ))
    })?;

    // The same ceiling chat applies: the addressed tier's value, else the
    // provider's flat timeout (never shorter than a chat call to that tier).
    let provider_timeout = state
        .settings
        .providers
        .get(&provider_name)
        .map(|c| c.timeout_secs)
        .unwrap_or(state.settings.tier_timeouts.deep);
    let timeout_secs = state.settings.tier_timeouts.resolve(&requested_model, provider_timeout);

    let span = tracing::Span::current();
    span.record("user_id", user.id);
    span.record("model", canonical_model.as_str());
    span.record("provider", provider_name.as_str());

    let req = DecisionRequest {
        model: canonical_model.clone(),
        body: body.clone(),
        timeout: std::time::Duration::from_secs(timeout_secs.max(1)),
    };
    let start = Instant::now();
    let result = adapter.decide(&req).await.map_err(map_decision_error)?;
    let latency_ms = start.elapsed().as_millis() as i64;

    let cost = state
        .cost_calc
        .calculate(&canonical_model, result.prompt_tokens, result.completion_tokens);
    span.record("cost.usd", cost);
    span.record("tokens.prompt", result.prompt_tokens as u64);
    span.record("tokens.completion", result.completion_tokens as u64);

    #[cfg(feature = "otel")]
    {
        crate::telemetry::metrics::record_request(&canonical_model, &provider_name, "ok");
        crate::telemetry::metrics::record_tokens(
            &canonical_model, &provider_name,
            result.prompt_tokens, result.completion_tokens,
        );
        crate::telemetry::metrics::record_cost(&canonical_model, &provider_name, user.id, cost);
        crate::telemetry::metrics::record_duration(&canonical_model, &provider_name, false, latency_ms as f64);
    }

    #[cfg(feature = "prometheus")]
    if let Some(ref metrics) = state.app_metrics {
        metrics.record_request(&canonical_model, &provider_name, "ok");
        metrics.record_tokens(&canonical_model, &provider_name, result.prompt_tokens, result.completion_tokens);
        metrics.record_cost(&canonical_model, &provider_name, cost);
    }

    // Fire-and-forget cost recording, as on the other priced routes. The
    // prompt row carries no content: the document and the questions stay
    // with the caller and the provider.
    let state_clone = state.clone();
    let request_model = requested_model.clone();
    let canonical_clone = canonical_model.clone();
    let provider_clone = provider_name.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    let user_project = attribution.project_or(user.api_key_project.clone());
    let prompt_tokens = result.prompt_tokens;
    let completion_tokens = result.completion_tokens;
    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model,
            routed_model: canonical_clone.clone(),
            provider: provider_clone.clone(),
            messages: "[]".to_string(),
            response: None,
            finish_reason: None,
            prompt_tokens: prompt_tokens as i64,
            completion_tokens: completion_tokens as i64,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: cost,
            latency_ms: Some(latency_ms),
            ttft_ms: None,
            attempts: Some(1),
            tags: "[]".to_string(),
            project: user_project.clone(),
            attribution_correlation_id: attr_correlation.clone(),
            attribution_tags: attr_tags.clone(),
            experiment_id: None,
            experiment_variant: None,
        };
        let storage = state_clone.storage.load();
        let stored = match crate::db::prompt_store::apply_storage_policy(&storage, prompt) {
            Some(p) => match PromptRepository::create(&*state_clone.prompt_db, p).await {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::error!("Failed to record decision prompt: {}", e);
                    None
                }
            },
            None => None,
        };
        let ledger = NewCostLedgerEntry {
            user_id,
            prompt_id: stored.as_ref().map(|s| s.id),
            model: canonical_clone,
            provider: provider_clone,
            project: user_project,
            tokens_in: prompt_tokens as i64,
            tokens_out: completion_tokens as i64,
            cost_usd: cost,
            api_key_id,
            attribution_correlation_id: attr_correlation,
            attribution_tags: attr_tags,
            experiment_id: None,
            experiment_variant: None,
            tokens_estimated: false,
        };
        if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
            tracing::error!("Failed to record decision cost: {}", e);
        }
    });

    // The provider's body, with the router's account of the call added.
    let meta = RouterMeta {
        requested_model,
        model: canonical_model.clone(),
        provider: provider_name,
        settings: serde_json::json!({
            "questions": body["questions"].as_array().map(|q| q.len()).unwrap_or(0),
            "timeout_secs": timeout_secs,
        }),
        tokens: Some(TokenMeta::new(result.prompt_tokens, result.completion_tokens)),
        results: None,
        cost: CallCost::spent(cost).priced_if(state.cost_calc.has_price(&canonical_model)),
        timing: TimingMeta {
            total_ms: received.elapsed().as_millis() as i64,
            latency_ms,
            provider_ms: Some(latency_ms),
            ttft_ms: None,
            attempts: 1,
            fallbacks: 0,
        },
    };
    let mut response = result.body;
    if !response.is_object() {
        response = serde_json::json!({ "result": response });
    }
    if !response["usage"].is_object() {
        response["usage"] = serde_json::json!({
            "prompt_tokens": result.prompt_tokens,
            "completion_tokens": result.completion_tokens,
            "total_tokens": result.prompt_tokens + result.completion_tokens,
        });
    }
    meta.cost.write_into(&mut response["usage"]);
    meta.attach(&mut response);
    Ok(Json(response).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{ProviderConfig, Settings};
    use std::collections::HashMap;

    fn router(strict: bool) -> RequestRouter {
        let mut settings = Settings::default();
        settings.routing.default_model = "foundry/chat-default".to_string();
        settings.routing.strict_model_resolution = strict;
        settings.routing.model_aliases.insert("decide".to_string(), "foundry/decision-a".to_string());
        RequestRouter::new(Arc::new(settings))
    }

    fn registry() -> DecisionRegistry {
        let mut configs = HashMap::new();
        configs.insert(
            "foundry".to_string(),
            ProviderConfig { decision_deployments: vec!["decision-only".into()], ..Default::default() },
        );
        DecisionRegistry::new(configs)
    }

    #[test]
    fn an_alias_and_a_prefixed_name_resolve_as_on_chat() {
        let (r, g) = (router(false), registry());
        assert_eq!(resolve_decision_model(&r, &g, "decide", "decide").unwrap(), ("foundry".to_string(), "decision-a".to_string()));
        assert_eq!(
            resolve_decision_model(&r, &g, "foundry/anything", "foundry/anything").unwrap(),
            ("foundry".to_string(), "anything".to_string())
        );
    }

    #[test]
    fn a_bare_decision_deployment_resolves_to_itself_through_decision_deployments() {
        let (r, g) = (router(false), registry());
        assert_eq!(
            resolve_decision_model(&r, &g, "decision-only", "decision-only").unwrap(),
            ("foundry".to_string(), "decision-only".to_string())
        );
    }

    #[test]
    fn an_unlisted_bare_name_is_a_400_naming_it_whatever_strict_model_resolution_says() {
        for strict in [false, true] {
            let (r, g) = (router(strict), registry());
            match resolve_decision_model(&r, &g, "gpt-6-nova", "gpt-6-nova") {
                Err(ApiError::InvalidRequest(msg)) => {
                    assert!(msg.contains("'gpt-6-nova'"), "{msg}");
                    assert!(msg.contains("decision_deployments"), "{msg}");
                    assert!(!msg.contains("chat-default"), "the default model is not offered: {msg}");
                }
                other => panic!("strict={strict}: expected InvalidRequest, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_scoped_alias_that_does_not_resolve_names_both_spellings() {
        let (r, g) = (router(false), registry());
        match resolve_decision_model(&r, &g, "decide-for-team", "team-decision") {
            Err(ApiError::InvalidRequest(msg)) => {
                assert!(msg.contains("'decide-for-team'") && msg.contains("'team-decision'"), "{msg}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn a_provider_rejection_becomes_the_callers_status_with_the_upstream_body() {
        let err = anyhow::Error::new(UpstreamRejected::from_response(400, r#"{"error":{"code":"no_decision_capability"}}"#));
        match map_decision_error(err) {
            ApiError::Refused { status, body, reason } => {
                assert_eq!(status, 400);
                assert_eq!(body["error"]["code"], "upstream_rejected");
                assert_eq!(body["error"]["upstream_status"], 400);
                assert_eq!(body["error"]["upstream"]["error"]["code"], "no_decision_capability");
                assert!(reason.contains("HTTP 400"), "{reason}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn a_preflight_refusal_is_a_400_and_anything_else_a_provider_error() {
        match map_decision_error(anyhow::Error::new(DecisionRequestInvalid("not a decision deployment".into()))) {
            ApiError::InvalidRequest(msg) => assert_eq!(msg, "not a decision deployment"),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
        assert!(matches!(map_decision_error(anyhow::anyhow!("connection reset")), ApiError::ProviderError(_)));
    }
}
