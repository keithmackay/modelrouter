//! `POST /v1/rerank`: candidate documents ordered by their relevance to a
//! query, by a reranker deployment, passed through the router.
//!
//! The router does what it does for every priced call and nothing else: it
//! authenticates the caller, applies the policy, resolves `model` through the
//! alias and scoped-alias maps to a `provider/deployment`, applies the tier
//! timeout ceiling, prices the call from the provider's billed search units at
//! the flat `rerank/{deployment}` rate, writes the prompt and cost-ledger rows,
//! and returns the provider's body as it arrived with `usage.cost_usd` and
//! `x_router` added. See `providers::rerank`.
//!
//! No retry and no fallback: a rerank names one deployment, and a provider's
//! 4xx is the provider refusing this request, returned to the caller with the
//! provider's status and body. The request binds to an experiment variant
//! exactly as decisions do (the overlay pins the requested reranker), every
//! row is stamped `call_kind = rerank`, and the call logs its lifecycle.

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
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError, request_lifecycle::UpstreamCall},
    db::models::{NewCostLedgerEntry, NewPrompt},
    providers::rerank::{result_count, RerankRequest, RerankRequestInvalid, UpstreamRejected},
    providers::rerank_registry::RerankRegistry,
    router::engine::RequestRouter,
    router::policy::PolicyDecision,
};

const ENDPOINT: &str = "/v1/rerank";

pub async fn rerank(
    State(state): State<AppState>,
    Extension(registry): Extension<Arc<RerankRegistry>>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!(
        "rerank",
        user_id = tracing::field::Empty,
        model = tracing::field::Empty,
        provider = tracing::field::Empty,
        "cost.usd" = tracing::field::Empty,
        "results.count" = tracing::field::Empty,
    );
    // See api::failure_log: capture wraps the whole handler so no error return
    // can escape being recorded.
    let ctx = crate::api::failure_log::context_from_request(ENDPOINT, &state, &user.0, &headers, &body);
    let started = Instant::now();

    let result = rerank_inner(State(state.clone()), registry, user, headers, Json(body)).instrument(span).await;

    if let Err(err) = &result {
        let ctx = crate::api::failure_log::FailureContext { latency_ms: Some(started.elapsed().as_millis() as i64), ..ctx };
        crate::api::failure_log::record_failure(&state, ctx, err).await;
    }
    result
}

/// The pricing key for a reranker: a flat dollars-per-search-unit rate under
/// `rerank/{deployment}` in `[[pricing]]` (or the live price feed), read the
/// way `search/{engine}` is. Unpriced: $0, warned about once, reported
/// `priced: false` so a caller never mistakes it for free.
pub fn rerank_pricing_key(deployment: &str) -> String {
    format!("rerank/{deployment}")
}

fn rerank_rate(state: &AppState, deployment: &str) -> Option<f64> {
    let key = rerank_pricing_key(deployment);
    let rate = state.cost_calc.flat_rate(&key);
    if rate.is_none() {
        state.cost_calc.note_unpriced(&key);
    }
    rate
}

/// Resolve a rerank request's `model` to `(provider, deployment)` without ever
/// substituting another model: the same rule as decisions. An alias or
/// `provider/deployment` resolves as on chat; a bare name resolves through the
/// providers' `rerank_deployments`; anything else is a 400 naming it, before
/// anything is sent, whatever `strict_model_resolution` says.
pub fn resolve_rerank_model(
    router: &RequestRouter,
    registry: &RerankRegistry,
    requested_model: &str,
    model: &str,
) -> Result<(String, String), ApiError> {
    let resolution = router.resolve_detailed(model);
    if !resolution.substituted {
        return Ok((resolution.provider, resolution.model));
    }
    if let Some(provider) = registry.provider_for_deployment(model) {
        tracing::debug!(requested_model, provider = provider.as_str(), "rerank: bare model resolved through the provider's rerank_deployments");
        return Ok((provider, model.trim().to_string()));
    }
    let scoped = if model != requested_model { format!(" (scoped alias '{model}')") } else { String::new() };
    tracing::warn!(
        requested_model,
        scoped_model = model,
        would_have_substituted = %format!("{}/{}", resolution.provider, resolution.model),
        "rerank: model does not resolve and no provider lists it as a rerank deployment; refusing rather than substituting"
    );
    Err(ApiError::InvalidRequest(format!(
        "model '{requested_model}'{scoped} is not a configured alias, names no provider, and is not one of any \
         provider's rerank_deployments; a rerank is never answered by a substitute model \
         (routing.default_model and routing.strict_model_resolution do not apply to /v1/rerank). \
         Add the deployment to [providers.<provider>] rerank_deployments, alias it in \
         [routing.model_aliases], or request it as 'provider/deployment'."
    )))
}

/// The two fields every rerank API takes, checked before any call so a
/// malformed request is the caller's 400 and never a priced provider call.
pub fn check_rerank_shape(body: &Value) -> Result<(usize, Option<u64>), ApiError> {
    if !body.is_object() {
        return Err(ApiError::InvalidRequest("the request body must be a JSON object".to_string()));
    }
    if body["query"].as_str().map(str::trim).filter(|q| !q.is_empty()).is_none() {
        return Err(ApiError::InvalidRequest("`query` is required: the text the documents are ranked against".to_string()));
    }
    let documents = body["documents"]
        .as_array()
        .filter(|d| !d.is_empty())
        .ok_or_else(|| ApiError::InvalidRequest("`documents` is required: a non-empty array of the texts to rank".to_string()))?;
    Ok((documents.len(), body["top_n"].as_u64()))
}

/// What the prompt log keeps of a rerank request: the query and the
/// documents, as a JSON object. `model` is on the row already; anything else
/// the caller sent is passed through to the provider, not kept here.
pub fn rerank_request_content(body: &Value) -> String {
    serde_json::json!({
        "query": body.get("query").cloned().unwrap_or(Value::Null),
        "documents": body.get("documents").cloned().unwrap_or(Value::Null),
        "top_n": body.get("top_n").cloned().unwrap_or(Value::Null),
    })
    .to_string()
}

/// What the prompt log keeps of the provider's answer: its `results` array as
/// it arrived, or the whole body when it carried none.
pub fn rerank_results_content(response: &Value) -> String {
    match response.get("results") {
        Some(results) => serde_json::json!({ "results": results }).to_string(),
        None => response.to_string(),
    }
}

/// The body the caller receives for a provider's 4xx: the router's stable
/// code, the provider's status, and the provider's body under `upstream`.
pub fn upstream_rejected_body(rejected: &UpstreamRejected) -> Value {
    serde_json::json!({
        "error": {
            "message": format!("the provider rejected the rerank request with HTTP {}", rejected.status),
            "type": "upstream_rejected",
            "code": "upstream_rejected",
            "upstream_status": rejected.status,
            "upstream": rejected.body,
        }
    })
}

/// Map a failed provider call: a typed 4xx passes through with its status, a
/// pre-flight refusal is the caller's 400, anything else is a provider error.
fn map_rerank_error(err: anyhow::Error) -> ApiError {
    if let Some(rejected) = UpstreamRejected::find(&err) {
        return ApiError::Refused { status: rejected.status, body: upstream_rejected_body(rejected), reason: rejected.to_string() };
    }
    if let Some(invalid) = RerankRequestInvalid::find(&err) {
        return ApiError::InvalidRequest(invalid.0.clone());
    }
    ApiError::ProviderError(err)
}

async fn rerank_inner(
    State(state): State<AppState>,
    registry: Arc<RerankRegistry>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    // Request receipt: `x_router.timing.total_ms` is measured from here.
    let received = Instant::now();
    let user = user.0;
    let attribution = crate::api::attribution::Attribution::extract(&body, &headers)?;
    let attr_correlation = attribution.correlation_id.clone();
    let attr_tags = attribution.tags_json();
    // Experiment binding, as on decisions: the variant's overlay pins the
    // requested reranker to a concrete `provider/deployment` and the rows are
    // stamped with the variant. `None` without the header.
    let binding = state
        .experiments
        .bind(&headers, &body, attribution.correlation_id.as_deref(), user.id, chrono::Utc::now().timestamp())
        .map_err(|e| super::experiment_bind_rejected(ENDPOINT, e))?;
    let experiment_id = binding.as_ref().map(|b| b.experiment_id);
    let experiment_variant = binding.as_ref().map(|b| b.variant.clone());
    let retain_content = binding.as_ref().is_some_and(|b| b.retain_content);

    let (document_count, top_n) = check_rerank_shape(&body)?;
    // No default model: a reranker is not interchangeable, and a caller that
    // did not name one gets told so.
    let requested_model = body["model"]
        .as_str()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| ApiError::InvalidRequest("`model` is required: the reranker deployment, or an alias for one".to_string()))?
        .to_string();
    let model = match &binding {
        Some(b) => b.overlay.get(&requested_model).cloned().unwrap_or_else(|| requested_model.clone()),
        None => state.router.scoped_name(&attribution.tags, &requested_model).unwrap_or_else(|| requested_model.clone()),
    };
    if let Some(b) = &binding {
        tracing::info!(
            experiment_id = b.experiment_id,
            variant = b.variant.as_str(),
            requested_model = requested_model.as_str(),
            model = model.as_str(),
            "rerank request bound to experiment variant"
        );
    }

    let policy_result = state.policy.check(&user, &model).await.map_err(|_| ApiError::Internal)?;
    match policy_result {
        PolicyDecision::Allow { .. } => {}
        PolicyDecision::Deny { reason, status, .. } => return Err(ApiError::PolicyDenied { reason, status }),
    }

    let (provider_name, canonical_model) = resolve_rerank_model(&state.router, &registry, &requested_model, &model)?;
    state.router.check_available(&provider_name, &canonical_model)?;

    // A model that resolves to a provider with no rerank surface (or an
    // unconfigured one) is the caller's error to read, not a gateway fault.
    let adapter = registry.get(&provider_name).map_err(|e| {
        ApiError::InvalidRequest(format!("model '{requested_model}' resolves to {provider_name}/{canonical_model}, which cannot rerank: {e}"))
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

    let req = RerankRequest { model: canonical_model.clone(), body: body.clone(), timeout: std::time::Duration::from_secs(timeout_secs.max(1)) };
    let request_id = format!("rerank-mr-{}", uuid::Uuid::new_v4());
    let mut upstream = UpstreamCall::start(&request_id, attribution.correlation_id.as_deref(), &provider_name, &canonical_model, false);
    let start = Instant::now();
    let result = match adapter.rerank(&req).await {
        Ok(result) => {
            upstream.finish("completed", None);
            result
        }
        Err(err) => {
            upstream.finish("failed", Some(&err.to_string()));
            return Err(map_rerank_error(err));
        }
    };
    let latency_ms = start.elapsed().as_millis() as i64;

    // Priced per billed search unit at the flat `rerank/{deployment}` rate.
    let rate = rerank_rate(&state, &canonical_model);
    let cost = f64::from(result.search_units) * rate.unwrap_or(0.0);
    let results_returned = result_count(&result.body) as i64;
    span.record("cost.usd", cost);
    span.record("results.count", results_returned);
    tracing::info!(
        request_id = request_id.as_str(),
        correlation_id = attribution.correlation_id.as_deref().unwrap_or(""),
        provider = provider_name.as_str(),
        model = canonical_model.as_str(),
        requested_model = requested_model.as_str(),
        experiment_id = experiment_id.unwrap_or(0),
        experiment_variant = experiment_variant.as_deref().unwrap_or(""),
        latency_ms,
        documents = document_count,
        top_n = top_n.unwrap_or(0),
        results = results_returned,
        search_units = result.search_units,
        input_tokens = result.input_tokens,
        cost_usd = cost,
        priced = rate.is_some(),
        outcome = "completed",
        "rerank request finished"
    );

    #[cfg(feature = "otel")]
    {
        crate::telemetry::metrics::record_request(&canonical_model, &provider_name, "ok");
        crate::telemetry::metrics::record_cost(&canonical_model, &provider_name, user.id, cost);
        crate::telemetry::metrics::record_duration(&canonical_model, &provider_name, false, latency_ms as f64);
    }

    #[cfg(feature = "prometheus")]
    if let Some(ref metrics) = state.app_metrics {
        metrics.record_request(&canonical_model, &provider_name, "ok");
        metrics.record_cost(&canonical_model, &provider_name, cost);
    }

    // Fire-and-forget cost recording, as on the other priced routes. The
    // prompt row keeps the query and documents and the provider's results,
    // subject to the storage policy below.
    let state_clone = state.clone();
    let request_model = requested_model.clone();
    let canonical_clone = canonical_model.clone();
    let provider_clone = provider_name.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    let user_project = attribution.project_or(user.api_key_project.clone());
    let input_tokens = i64::from(result.input_tokens);
    let request_content = rerank_request_content(&body);
    let results_content = rerank_results_content(&result.body);
    let experiment_id_row = experiment_id;
    let experiment_variant_row = experiment_variant.clone();
    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model,
            routed_model: canonical_clone.clone(),
            provider: provider_clone.clone(),
            messages: request_content,
            response: Some(results_content),
            finish_reason: None,
            prompt_tokens: input_tokens,
            completion_tokens: 0,
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
            experiment_id: experiment_id_row,
            experiment_variant: experiment_variant_row.clone(),
            call_kind: crate::db::models::CallKind::Rerank,
        };
        let storage = {
            let global = state_clone.storage.load();
            if retain_content && global.allows_forced_content() {
                crate::config::schema::StorageConfig { store_prompts: true, store_prompt_content: true, ..(**global).clone() }
            } else {
                (**global).clone()
            }
        };
        let stored = match crate::db::prompt_store::apply_storage_policy(&storage, prompt) {
            Some(p) => match PromptRepository::create(&*state_clone.prompt_db, p).await {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::error!("Failed to record rerank prompt: {}", e);
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
            tokens_in: input_tokens,
            tokens_out: 0,
            cost_usd: cost,
            api_key_id,
            attribution_correlation_id: attr_correlation,
            attribution_tags: attr_tags,
            experiment_id: experiment_id_row,
            experiment_variant: experiment_variant_row,
            call_kind: crate::db::models::CallKind::Rerank,
            tokens_estimated: false,
        };
        if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
            tracing::error!("Failed to record rerank cost: {}", e);
        }
    });

    // The provider's body, with the router's account of the call added.
    let meta = RouterMeta {
        requested_model,
        model: canonical_model.clone(),
        provider: provider_name,
        settings: serde_json::json!({
            "documents": document_count,
            "top_n": top_n,
            "search_units": result.search_units,
            "timeout_secs": timeout_secs,
            "experiment_id": experiment_id,
            "experiment_variant": experiment_variant,
        }),
        tokens: (result.input_tokens > 0).then(|| TokenMeta::new(result.input_tokens, 0)),
        results: Some(results_returned),
        cost: CallCost::spent(cost).priced_if(rate.is_some()),
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
        response["usage"] = serde_json::json!({ "search_units": result.search_units, "prompt_tokens": result.input_tokens });
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
        settings.routing.model_aliases.insert("rerank".to_string(), "foundry/rerank-a".to_string());
        RequestRouter::new(Arc::new(settings))
    }

    fn registry() -> RerankRegistry {
        let mut configs = HashMap::new();
        configs.insert("foundry".to_string(), ProviderConfig { rerank_deployments: vec!["rerank-only".into()], ..Default::default() });
        RerankRegistry::new(configs)
    }

    #[test]
    fn an_alias_a_prefixed_name_and_a_bare_rerank_deployment_resolve_without_substitution() {
        let (r, g) = (router(false), registry());
        assert_eq!(resolve_rerank_model(&r, &g, "rerank", "rerank").unwrap(), ("foundry".to_string(), "rerank-a".to_string()));
        assert_eq!(resolve_rerank_model(&r, &g, "foundry/x", "foundry/x").unwrap(), ("foundry".to_string(), "x".to_string()));
        assert_eq!(resolve_rerank_model(&r, &g, "rerank-only", "rerank-only").unwrap(), ("foundry".to_string(), "rerank-only".to_string()));
    }

    #[test]
    fn an_unlisted_bare_name_is_a_400_naming_it_whatever_strict_model_resolution_says() {
        for strict in [false, true] {
            let (r, g) = (router(strict), registry());
            match resolve_rerank_model(&r, &g, "gpt-6-nova", "gpt-6-nova") {
                Err(ApiError::InvalidRequest(msg)) => {
                    assert!(msg.contains("'gpt-6-nova'") && msg.contains("rerank_deployments"), "{msg}");
                    assert!(!msg.contains("chat-default"), "the default model is not offered: {msg}");
                }
                other => panic!("strict={strict}: expected InvalidRequest, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_shape_check_wants_a_query_and_documents_before_any_call() {
        let ok = serde_json::json!({"model": "m", "query": "q", "documents": ["a", "b", "c"], "top_n": 2});
        assert_eq!(check_rerank_shape(&ok).unwrap(), (3, Some(2)));
        for (body, needle) in [
            (serde_json::json!([]), "JSON object"),
            (serde_json::json!({"model": "m", "documents": ["a"]}), "`query`"),
            (serde_json::json!({"model": "m", "query": "  ", "documents": ["a"]}), "`query`"),
            (serde_json::json!({"model": "m", "query": "q"}), "`documents`"),
            (serde_json::json!({"model": "m", "query": "q", "documents": []}), "`documents`"),
        ] {
            match check_rerank_shape(&body) {
                Err(ApiError::InvalidRequest(msg)) => assert!(msg.contains(needle), "{msg}"),
                other => panic!("expected InvalidRequest for {body}, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_prompt_log_keeps_the_query_documents_and_results() {
        let body = serde_json::json!({"model": "rerank", "query": "q", "documents": ["a"], "top_n": 1, "session_id": "s"});
        let kept: Value = serde_json::from_str(&rerank_request_content(&body)).unwrap();
        assert_eq!(kept["query"], "q");
        assert_eq!(kept["documents"][0], "a");
        assert!(kept.get("model").is_none() && kept.get("session_id").is_none());
        let response = serde_json::json!({"id": "r", "results": [{"index": 0, "relevance_score": 0.9}], "meta": {}});
        let kept: Value = serde_json::from_str(&rerank_results_content(&response)).unwrap();
        assert_eq!(kept["results"][0]["relevance_score"], 0.9);
        assert!(kept.get("meta").is_none());
        assert_eq!(rerank_results_content(&serde_json::json!({"result": 1})), r#"{"result":1}"#);
    }

    #[test]
    fn a_provider_rejection_keeps_its_status_and_a_preflight_refusal_is_a_400() {
        let err = anyhow::Error::new(UpstreamRejected::from_response(400, r#"{"error":{"code":"not_a_reranker"}}"#));
        match map_rerank_error(err) {
            ApiError::Refused { status, body, reason } => {
                assert_eq!(status, 400);
                assert_eq!(body["error"]["code"], "upstream_rejected");
                assert_eq!(body["error"]["upstream"]["error"]["code"], "not_a_reranker");
                assert!(reason.contains("HTTP 400"), "{reason}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        match map_rerank_error(anyhow::Error::new(RerankRequestInvalid("not a rerank deployment".into()))) {
            ApiError::InvalidRequest(msg) => assert_eq!(msg, "not a rerank deployment"),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
        assert!(matches!(map_rerank_error(anyhow::anyhow!("connection reset")), ApiError::ProviderError(_)));
        assert_eq!(rerank_pricing_key("Cohere-rerank-v4.0-pro"), "rerank/Cohere-rerank-v4.0-pro");
    }
}
