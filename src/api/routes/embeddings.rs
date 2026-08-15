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
    headers: axum::http::HeaderMap,
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
    // See api::failure_log: capture wraps the whole handler so no error return
    // can escape being recorded.
    let ctx = crate::api::failure_log::context_from_request(
        "/v1/embeddings",
        &state,
        &user.0,
        &headers,
        &body,
    );
    let started = Instant::now();

    let result = embeddings_inner(State(state.clone()), user, headers, Json(body))
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

async fn embeddings_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;
    let attribution = crate::api::attribution::Attribution::extract(&body, &headers)?;
    let attr_correlation = attribution.correlation_id.clone();
    let attr_tags = attribution.tags_json();
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

    // Honour the caller's pinned width across every provider we may fall back to.
    let dimensions = body["dimensions"].as_u64().map(|d| d as u32);
    // OpenAI's own default is "float" only when the field is absent from the wire;
    // its SDKs put "base64" on the wire explicitly, so read what was actually sent.
    let encoding_format = body["encoding_format"]
        .as_str()
        .unwrap_or("float")
        .to_string();
    if encoding_format != "float" && encoding_format != "base64" {
        return Err(ApiError::InvalidRequest(format!(
            "unsupported encoding_format '{}': expected 'float' or 'base64'",
            encoding_format
        )));
    }

    // Operator disable gate (issue #5) — 403 naming the reason, never a provider
    // call. Checked on what the CALLER asked for, before any fallback: a model an
    // operator has taken out of rotation must be refused, not quietly answered by
    // a different one. Disabled fallback candidates are skipped inside the loop
    // below, so a disable is never routed around.
    {
        let (p, m) = state.router.resolve(&model);
        state.router.check_available(&p, &m)?;
    }

    crate::api::routes::guard_model_substitution(&state, &model)?;

    // Walk the fallback chain, exactly as the completions path does. Embeddings
    // used to fail outright at `registry.get()` with no failover at all, so a
    // single unavailable embedding provider took down every caller that depended
    // on it — including callers whose config named a perfectly good alternative.
    //
    // A missing ADAPTER is treated as a failure worth falling back from, not just
    // a failed call: "no adapter for this provider" is the most common way an
    // embedding request dies, and it is precisely the case an alternative fixes.
    let span = tracing::Span::current();
    span.record("user_id", user.id);

    let start = Instant::now();
    let (provider_name, canonical_model, result) = {
        let (mut current_provider, mut current_model) = state.router.resolve(&model);
        // Bounded: `next_after` follows configured chains, and a chain that points
        // back at itself (a→b, b→a) would otherwise spin forever, burning provider
        // calls on every hop. The limit converts a config mistake into a bounded
        // error instead of a hung request.
        const MAX_FALLBACK_HOPS: usize = 8;
        let mut hops = 0usize;
        loop {
            // A disabled candidate is not called. Treated as a failed attempt so
            // the walk continues to the next entry rather than 403-ing a request
            // whose PRIMARY model was perfectly available.
            let attempt = match state.router.check_available(&current_provider, &current_model) {
                Err(unavailable) => Err(anyhow::anyhow!("{}", unavailable.message())),
                Ok(()) => match state.embedding_registry.get(&current_provider) {
                Ok(adapter) => {
                    let req = EmbeddingRequest {
                        model: current_model.clone(),
                        input: input.clone(),
                        dimensions,
                    };
                    adapter.embed(&req).await
                }
                Err(e) => Err(e),
                },
            };

            match attempt {
                Ok(r) => break (current_provider, current_model, r),
                Err(e) => {
                    tracing::warn!(
                        model = current_model.as_str(),
                        provider = current_provider.as_str(),
                        error = %e,
                        "Embedding call failed, checking fallback chain"
                    );
                    if hops >= MAX_FALLBACK_HOPS {
                        tracing::error!(
                            model = current_model.as_str(),
                            hops,
                            "embedding fallback chain exceeded its hop limit — check \
                             [routing.fallback_chains] for a cycle"
                        );
                        return Err(ApiError::ProviderError(e));
                    }
                    match state.fallback.next_after(&current_model) {
                        Some(next_model) => {
                            hops += 1;
                            let (next_provider, next_canonical) =
                                state.router.resolve(&next_model);
                            current_provider = next_provider;
                            current_model = next_canonical;
                            tracing::info!(
                                fallback_model = current_model.as_str(),
                                "Retrying embedding with fallback"
                            );
                        }
                        None => return Err(ApiError::ProviderError(e)),
                    }
                }
            }
        }
    };
    let latency_ms = start.elapsed().as_millis() as i64;

    span.record("model", canonical_model.as_str());
    span.record("provider", provider_name.as_str());

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
    let user_project = attribution.project_or(user.api_key_project.clone());
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
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: cost,
            latency_ms: Some(latency_ms),
            tags: "[]".to_string(),
            project: user_project.clone(),
            attribution_correlation_id: attr_correlation.clone(),
            attribution_tags: attr_tags.clone(),
        };
        // Storage policy (issue #4): the prompt row is optional; the cost row is not.
        let stored = match crate::db::prompt_store::apply_storage_policy(&state_clone.storage.load(), prompt) {
            Some(p) => match PromptRepository::create(&*state_clone.db, p).await {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::error!("Failed to record embedding prompt: {}", e);
                    None
                }
            },
            None => None,
        };
        {
                let ledger = NewCostLedgerEntry {
                    user_id,
                    prompt_id: stored.as_ref().map(|s| s.id),
                    model: canonical_clone,
                    provider: provider_clone,
                    project: user_project.clone(),
                    tokens_in: prompt_tokens as i64,
                    tokens_out: 0,
                    cost_usd: cost,
                    api_key_id,
                    attribution_correlation_id: attr_correlation.clone(),
                    attribution_tags: attr_tags.clone(),
                };
                if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
                    tracing::error!("Failed to record embedding cost: {}", e);
                }
        }
    });

    // Build OpenAI-compatible response.
    //
    // `encoding_format` MUST be honoured. The official OpenAI SDKs default to
    // "base64" when the caller does not say otherwise, and then decode whatever
    // comes back as base64. Returning a float array to a client that asked for
    // base64 does not fail — the JS SDK runs Buffer.from(array, 'base64') over
    // it, which yields a shorter vector of zeros. Observed here: a 768-dimension
    // request came back to the client as 192 zeros, silently, ready to be stored
    // and to corrupt every similarity comparison made against it afterwards.
    let data: Vec<Value> = result
        .embeddings
        .iter()
        .enumerate()
        .map(|(i, emb)| {
            let embedding = if encoding_format == "base64" {
                use base64::Engine;
                let mut bytes = Vec::with_capacity(emb.len() * 4);
                for f in emb {
                    bytes.extend_from_slice(&f.to_le_bytes());
                }
                Value::String(base64::engine::general_purpose::STANDARD.encode(bytes))
            } else {
                serde_json::json!(emb)
            };
            serde_json::json!({
                "object": "embedding",
                "index": i,
                "embedding": embedding,
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
