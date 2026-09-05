//! Liveness and deep health.
//!
//! `GET /health` stays a cheap liveness probe: `status` is "ok" whenever the
//! process serves. When caching is enabled it additionally reports the cache
//! backend's live state — the exact signal whose absence let a "cache
//! unreachable" belief persist invisibly for days (issue #21). A down cache is
//! NOT unhealthy: requests are served live, so `status` stays "ok".
//!
//! `GET /health/deep` (issue #20) additionally proves that LLM, embedding, and
//! search calls THROUGH the gateway work, by issuing one minimal real call per
//! capability along the normal routing path (resolution, availability gates,
//! provider credentials). Results are cached in-process for
//! `health.deep_ttl_seconds` so polling cannot burn provider quota.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{extract::State, Extension, Json};
use serde_json::{json, Value};

use crate::api::app::AppState;
use crate::providers::adapter::NormalizedRequest;
use crate::providers::embedding::EmbeddingRequest;
use crate::providers::search::SearchRequest;

/// GET /health — liveness plus (when caching is enabled) cache-backend state.
pub async fn health_check(State(state): State<AppState>) -> Json<Value> {
    let mut body = json!({"status": "ok"});
    if let Some(cache) = cache_block(&state).await {
        body["cache"] = cache;
    }
    Json(body)
}

/// The `cache` block shared by /health and /health/deep. `None` when caching
/// is disabled — a disabled cache is a choice, not a health signal.
async fn cache_block(state: &AppState) -> Option<Value> {
    let cache = &state.response_cache;
    if !cache.enabled() {
        return None;
    }
    // `connected` is a live probe (PING for Redis) that also heals a dropped
    // connection; during the store's backoff window it returns false without
    // network I/O, so health polling stays cheap while Redis is down.
    let connected = cache.connected().await;
    let entries = if connected { cache.entry_count().await } else { 0 };
    Some(json!({
        "backend": cache.backend_name(),
        "connected": connected,
        "namespace": cache.namespace(),
        "entries": entries,
    }))
}

// ── Deep health ───────────────────────────────────────────────────────────────

/// In-process cache for the deep-health result, injected as an axum
/// `Extension` by `build_router`. The mutex is held across a probe run on
/// purpose: concurrent /health/deep calls wait for the in-flight probe instead
/// of each spending provider money.
#[derive(Clone, Default)]
pub struct DeepHealthCache(Arc<tokio::sync::Mutex<Option<(Instant, Value)>>>);

/// GET /health/deep
pub async fn deep_health(
    State(state): State<AppState>,
    Extension(cache): Extension<DeepHealthCache>,
) -> Json<Value> {
    let ttl = Duration::from_secs(state.settings.health.deep_ttl_seconds.max(1));
    let mut guard = cache.0.lock().await;
    if let Some((at, body)) = guard.as_ref() {
        if at.elapsed() < ttl {
            let mut body = body.clone();
            body["cached"] = json!(true);
            return Json(body);
        }
    }

    let (llm, embedding, search) = tokio::join!(
        probe_llm(&state),
        probe_embedding(&state),
        probe_search(&state),
    );

    let caps = [&llm, &embedding, &search];
    let failed = caps.iter().filter(|c| c.status == "failed").count();
    let configured = caps.iter().filter(|c| c.status != "skipped").count();
    let status = if failed == 0 {
        "ok"
    } else if failed == configured {
        "failed"
    } else {
        "degraded"
    };

    let mut body = json!({
        "status": status,
        "capabilities": {
            "llm": llm.to_json(),
            "embedding": embedding.to_json(),
            "search": search.to_json(),
        },
        "checked_at": chrono::Utc::now().timestamp(),
        "cached": false,
    });
    if let Some(cache_state) = cache_block(&state).await {
        body["cache"] = cache_state;
    }
    *guard = Some((Instant::now(), body.clone()));
    Json(body)
}

struct CapabilityReport {
    status: &'static str,
    target: String,
    latency_ms: i64,
    error: Option<String>,
}

impl CapabilityReport {
    fn ok(target: String, latency_ms: i64) -> Self {
        Self { status: "ok", target, latency_ms, error: None }
    }
    fn failed(target: String, latency_ms: i64, error: String) -> Self {
        Self { status: "failed", target, latency_ms, error: Some(error) }
    }
    fn skipped(target: String, reason: String) -> Self {
        Self { status: "skipped", target, latency_ms: 0, error: Some(reason) }
    }
    fn to_json(&self) -> Value {
        json!({
            "status": self.status,
            "target": self.target,
            "latency_ms": self.latency_ms,
            "error": self.error,
        })
    }
}

/// One-token completion on the default (or configured probe) model, resolved
/// through aliases and availability gates exactly like a caller's request.
async fn probe_llm(state: &AppState) -> CapabilityReport {
    let requested = state
        .settings
        .health
        .llm_probe_model
        .clone()
        .unwrap_or_else(|| state.settings.routing.default_model.clone());
    let (provider, model) = state.router.resolve(&requested);
    let target = format!("{}/{}", provider, model);

    let adapter = match state.provider_registry.get(&provider) {
        Ok(a) => a,
        Err(e) => {
            // No adapter AND no config for the provider means the capability
            // is simply not set up on this gateway — that is "skipped", not a
            // failure. An adapter that fails to build for a CONFIGURED
            // provider is a real fault.
            if state.settings.providers.contains_key(&provider) {
                return CapabilityReport::failed(target, 0, e.to_string());
            }
            return CapabilityReport::skipped(target, format!("no LLM provider configured: {}", e));
        }
    };
    if let Err(unavailable) = state.router.check_available(&provider, &model) {
        return CapabilityReport::failed(target, 0, unavailable.message());
    }

    let req = NormalizedRequest {
        model: model.clone(),
        messages: vec![json!({"role": "user", "content": "health probe"})],
        stream: false,
        temperature: None,
        max_tokens: Some(1),
        extra_params: json!({}),
    };
    let started = Instant::now();
    match adapter.complete(&req).await {
        Ok(result) => {
            let latency = started.elapsed().as_millis() as i64;
            let cost = state
                .cost_calc
                .calculate(&model, result.prompt_tokens, result.completion_tokens);
            record_probe_usage(
                state,
                model,
                provider,
                result.prompt_tokens as i64,
                result.completion_tokens as i64,
                cost,
            );
            CapabilityReport::ok(target, latency)
        }
        Err(e) => CapabilityReport::failed(target, started.elapsed().as_millis() as i64, e.to_string()),
    }
}

/// One short embedding through the embedding registry.
async fn probe_embedding(state: &AppState) -> CapabilityReport {
    let requested = state.settings.health.embedding_probe_model.clone();
    let (provider, model) = state.router.resolve(&requested);
    let target = format!("{}/{}", provider, model);

    let adapter = match state.embedding_registry.get(&provider) {
        Ok(a) => a,
        Err(e) => {
            if state.settings.providers.contains_key(&provider) {
                return CapabilityReport::failed(target, 0, e.to_string());
            }
            return CapabilityReport::skipped(
                target,
                format!("no embedding provider configured: {}", e),
            );
        }
    };
    if let Err(unavailable) = state.router.check_available(&provider, &model) {
        return CapabilityReport::failed(target, 0, unavailable.message());
    }

    let req = EmbeddingRequest {
        model: model.clone(),
        input: vec!["health probe".to_string()],
        dimensions: None,
    };
    let started = Instant::now();
    match adapter.embed(&req).await {
        Ok(result) => {
            let latency = started.elapsed().as_millis() as i64;
            let cost = state.cost_calc.calculate(&model, result.prompt_tokens, 0);
            record_probe_usage(state, model, provider, result.prompt_tokens as i64, 0, cost);
            CapabilityReport::ok(target, latency)
        }
        Err(e) => CapabilityReport::failed(target, started.elapsed().as_millis() as i64, e.to_string()),
    }
}

/// One single-result search on the configured probe engine.
async fn probe_search(state: &AppState) -> CapabilityReport {
    let engine = state.settings.health.search_probe_engine.clone();
    let target = format!("search/{}", engine);

    let adapter = match state.search_registry.get(&engine) {
        Ok(a) => a,
        Err(e) => {
            if state.settings.providers.contains_key(&engine) {
                return CapabilityReport::failed(target, 0, e.to_string());
            }
            return CapabilityReport::skipped(target, format!("no search engine configured: {}", e));
        }
    };

    let req = SearchRequest {
        query: "health probe".to_string(),
        max_results: Some(1),
    };
    let started = Instant::now();
    match adapter.search(&req).await {
        Ok(_) => {
            let latency = started.elapsed().as_millis() as i64;
            // Search is priced per query, mirroring api/routes/search.rs.
            let cost = state
                .settings
                .pricing
                .iter()
                .find(|p| p.model == target)
                .map(|p| p.input_per_million)
                .unwrap_or(0.005);
            record_probe_usage(state, target.clone(), engine, 0, 0, cost);
            CapabilityReport::ok(target, latency)
        }
        Err(e) => CapabilityReport::failed(target, started.elapsed().as_millis() as i64, e.to_string()),
    }
}

/// Probes are real provider calls and must appear in the ledger like any other
/// call, attributed to a stable system user so their spend is visible and
/// separable. Fire-and-forget, matching how the request routes record usage.
fn record_probe_usage(
    state: &AppState,
    model: String,
    provider: String,
    tokens_in: i64,
    tokens_out: i64,
    cost_usd: f64,
) {
    use crate::db::models::{NewCostLedgerEntry, NewUser};
    use crate::db::repositories::{costs::CostRepository, users::UserRepository};

    const PROBE_USER: &str = "health-probe";
    let state = state.clone();
    tokio::spawn(async move {
        let user = match UserRepository::find_by_name(&*state.db, PROBE_USER).await {
            Ok(Some(u)) => u,
            Ok(None) => {
                match UserRepository::create(
                    &*state.db,
                    NewUser { name: PROBE_USER.to_string(), email: None },
                )
                .await
                {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to create health-probe user");
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to look up health-probe user");
                return;
            }
        };
        let entry = NewCostLedgerEntry {
            user_id: user.id,
            prompt_id: None,
            model,
            provider,
            project: Some(PROBE_USER.to_string()),
            tokens_in,
            tokens_out,
            cost_usd,
            api_key_id: None,
            attribution_correlation_id: None,
            attribution_tags: "[]".to_string(),
            experiment_id: None,
            experiment_variant: None,
            tokens_estimated: false,
        };
        if let Err(e) = CostRepository::create(&*state.db, entry).await {
            tracing::error!(error = %e, "failed to record health-probe usage");
        }
    });
}
