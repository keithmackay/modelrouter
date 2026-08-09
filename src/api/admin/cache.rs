//! Admin surface for the response cache: JWT-gated REST under `/admin/api/cache/*`
//! and a dashboard page at `/admin/cache`.
//!
//! Reads (stats, policy) need any admin session; mutations (purge, policy
//! change) need a superadmin, and are audited.

use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect},
    Form, Json,
};
use serde::Deserialize;

use super::audit::audit;
use super::auth::{AdminSession, SuperAdminSession};
use super::dashboard::{DashboardError, DashboardSession};
use crate::api::{app::AppState, error::ApiError};
use crate::db::repositories::costs::{CacheUsageSummary, CostRepository};
use crate::router::cache::CachePolicyUpdate;

/// Lookback for the ledger-derived (cross-process, durable) hit rate.
const LEDGER_WINDOW_DAYS: i64 = 30;

fn ledger_since() -> String {
    (chrono::Utc::now() - chrono::Duration::days(LEDGER_WINDOW_DAYS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Combined live (in-process) and ledger-derived (durable) view.
async fn stats_json(state: &AppState) -> serde_json::Value {
    let live = state.response_cache.stats().await;
    let since = ledger_since();
    let ledger = CostRepository::cache_summary_since(&*state.db, None, &since)
        .await
        .unwrap_or_default();
    let by_model = CostRepository::cache_summary_by_model_since(&*state.db, &since)
        .await
        .unwrap_or_default();

    serde_json::json!({
        "live": live,
        "ledger": {
            "window_days": LEDGER_WINDOW_DAYS,
            "since": since,
            "hits": ledger.hits,
            "requests": ledger.requests,
            "hit_rate": ledger.hit_rate(),
            "saved_usd": ledger.saved_usd,
            "by_model": by_model.iter().map(|(model, s)| serde_json::json!({
                "model": model,
                "hits": s.hits,
                "requests": s.requests,
                "hit_rate": s.hit_rate(),
                "saved_usd": s.saved_usd,
            })).collect::<Vec<_>>(),
        },
    })
}

// ── REST API ──────────────────────────────────────────────────────────────────

/// GET /admin/api/cache/stats
pub async fn get_cache_stats(
    State(state): State<AppState>,
    _session: AdminSession,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(stats_json(&state).await))
}

/// GET /admin/api/cache/policy
pub async fn get_cache_policy(
    State(state): State<AppState>,
    _session: AdminSession,
) -> Result<Json<serde_json::Value>, ApiError> {
    let policy = state.response_cache.policy();
    Ok(Json(serde_json::to_value(&*policy).unwrap_or_default()))
}

/// PUT /admin/api/cache/policy — partial update, applied immediately.
///
/// Runtime only: the config file is not rewritten, so a restart returns to the
/// configured policy. That is deliberate — config stays the source of truth.
pub async fn put_cache_policy(
    State(state): State<AppState>,
    admin: SuperAdminSession,
    Json(update): Json<CachePolicyUpdate>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let before = serde_json::to_string(&*state.response_cache.policy()).ok();
    let policy = state.response_cache.update_policy(&update);
    let after = serde_json::to_string(&*policy).ok();
    audit(
        &state.db,
        Some(admin.0.sub),
        &admin.0.name,
        "cache.policy_update",
        Some("cache".to_string()),
        before,
        after,
    )
    .await;
    Ok(Json(serde_json::to_value(&*policy).unwrap_or_default()))
}

#[derive(Debug, Deserialize)]
pub struct PurgeRequest {
    /// `all` (default), `model`, or `key`.
    #[serde(default)]
    pub scope: Option<String>,
    pub model: Option<String>,
    pub key: Option<String>,
}

/// POST /admin/api/cache/purge
pub async fn post_cache_purge(
    State(state): State<AppState>,
    admin: SuperAdminSession,
    Json(req): Json<PurgeRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (scope, removed) = purge(&state, &req).await?;
    audit(
        &state.db,
        Some(admin.0.sub),
        &admin.0.name,
        "cache.purge",
        Some(scope.clone()),
        None,
        Some(format!("removed:{}", removed)),
    )
    .await;
    Ok(Json(serde_json::json!({ "scope": scope, "removed": removed })))
}

/// Shared purge logic for the REST and dashboard handlers.
async fn purge(state: &AppState, req: &PurgeRequest) -> Result<(String, u64), ApiError> {
    match req.scope.as_deref().unwrap_or("all") {
        "all" => Ok(("all".to_string(), state.response_cache.purge_all().await)),
        "model" => {
            let model = req.model.as_deref().filter(|m| !m.is_empty()).ok_or_else(|| {
                ApiError::InvalidRequest("scope=model requires a model".to_string())
            })?;
            Ok((
                format!("model:{}", model),
                state.response_cache.purge_model(model).await,
            ))
        }
        "key" => {
            let key = req.key.as_deref().filter(|k| !k.is_empty()).ok_or_else(|| {
                ApiError::InvalidRequest("scope=key requires a key".to_string())
            })?;
            Ok((
                format!("key:{}", key),
                state.response_cache.purge_key(key).await,
            ))
        }
        other => Err(ApiError::InvalidRequest(format!(
            "unknown purge scope: {}",
            other
        ))),
    }
}

// ── Dashboard page ────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct CachePageQuery {
    #[serde(default)]
    pub msg: Option<String>,
}

/// GET /admin/cache
pub async fn get_cache_page(
    State(state): State<AppState>,
    _session: DashboardSession,
    Query(q): Query<CachePageQuery>,
) -> Result<Html<String>, DashboardError> {
    let live = state.response_cache.stats().await;
    let policy = state.response_cache.policy();
    let since = ledger_since();
    let ledger: CacheUsageSummary = CostRepository::cache_summary_since(&*state.db, None, &since)
        .await
        .unwrap_or_default();
    let by_model = CostRepository::cache_summary_by_model_since(&*state.db, &since)
        .await
        .unwrap_or_default();
    let daily = daily_hit_series(&state, &since).await;

    let model_rows: Vec<minijinja::Value> = by_model
        .iter()
        .filter(|(_, s)| s.requests > 0)
        .map(|(model, s)| {
            minijinja::context! {
                model => model,
                hits => s.hits,
                requests => s.requests,
                hit_rate_pct => (s.hit_rate() * 100.0).round(),
                saved_usd => s.saved_usd,
            }
        })
        .collect();

    let live_models: Vec<minijinja::Value> = live
        .by_model
        .iter()
        .map(|m| {
            minijinja::context! {
                model => m.model.clone(),
                hits => m.hits,
                misses => m.misses,
                hit_rate_pct => (m.hit_rate * 100.0).round(),
                saved_usd => m.saved_usd,
            }
        })
        .collect();

    super::dashboard::render(
        "cache.html",
        minijinja::context! {
            msg => q.msg,
            backend => live.backend,
            healthy => live.healthy,
            enabled => live.enabled,
            entries => live.entries,
            evictions => live.evictions,
            stores => live.stores,
            live_hits => live.hits,
            live_misses => live.misses,
            live_hit_rate_pct => (live.hit_rate * 100.0).round(),
            live_saved_usd => live.saved_usd,
            live_models => live_models,
            window_days => LEDGER_WINDOW_DAYS,
            ledger_hits => ledger.hits,
            ledger_requests => ledger.requests,
            ledger_hit_rate_pct => (ledger.hit_rate() * 100.0).round(),
            ledger_saved_usd => ledger.saved_usd,
            model_rows => model_rows,
            daily => daily,
            policy => serde_json::to_value(&*policy).unwrap_or_default(),
        },
    )
}

/// Hit rate per calendar day over the ledger window, for the dashboard chart.
async fn daily_hit_series(state: &AppState, since: &str) -> Vec<minijinja::Value> {
    let end = (chrono::Utc::now() + chrono::Duration::days(1))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    CostRepository::cache_daily_series(&*state.db, since, &end)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(day, hits, requests)| {
            let rate = if requests == 0 {
                0.0
            } else {
                hits as f64 / requests as f64 * 100.0
            };
            minijinja::context! {
                day => day,
                hits => hits,
                requests => requests,
                hit_rate_pct => (rate * 10.0).round() / 10.0,
            }
        })
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct PurgeForm {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
}

/// POST /admin/cache/purge (dashboard form)
pub async fn post_cache_purge_page(
    State(state): State<AppState>,
    session: DashboardSession,
    Form(form): Form<PurgeForm>,
) -> Result<Redirect, DashboardError> {
    let req = PurgeRequest {
        scope: form.scope,
        model: form.model,
        key: form.key,
    };
    let (scope, removed) = purge(&state, &req)
        .await
        .map_err(|e| DashboardError::BadRequest(format!("{:?}", e)))?;
    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "cache.purge",
        Some(scope),
        None,
        Some(format!("removed:{}", removed)),
    )
    .await;
    Ok(Redirect::to(&format!(
        "/admin/cache?msg=Purged%20{}%20entries",
        removed
    )))
}

#[derive(Debug, Deserialize)]
pub struct PolicyForm {
    #[serde(default)]
    pub enabled: Option<String>,
    #[serde(default)]
    pub completions_enabled: Option<String>,
    #[serde(default)]
    pub search_enabled: Option<String>,
    pub completions_max_temperature: Option<f64>,
    pub completions_ttl_seconds: Option<u64>,
    pub search_ttl_seconds: Option<u64>,
}

/// POST /admin/cache/policy (dashboard form)
///
/// HTML checkboxes only submit when checked, so each toggle is read as
/// present/absent rather than as a boolean.
pub async fn post_cache_policy_page(
    State(state): State<AppState>,
    session: DashboardSession,
    Form(form): Form<PolicyForm>,
) -> Result<Redirect, DashboardError> {
    let before = serde_json::to_string(&*state.response_cache.policy()).ok();
    let update = CachePolicyUpdate {
        enabled: Some(form.enabled.is_some()),
        completions_enabled: Some(form.completions_enabled.is_some()),
        search_enabled: Some(form.search_enabled.is_some()),
        completions_max_temperature: form.completions_max_temperature,
        completions_ttl_seconds: form.completions_ttl_seconds,
        search_ttl_seconds: form.search_ttl_seconds,
        ..Default::default()
    };
    let policy = state.response_cache.update_policy(&update);
    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "cache.policy_update",
        Some("cache".to_string()),
        before,
        serde_json::to_string(&*policy).ok(),
    )
    .await;
    Ok(Redirect::to("/admin/cache?msg=Policy%20updated"))
}

/// Stable join key between a cost row and its cache aggregates.
pub fn cost_row_key(
    user_id: i64,
    model: &str,
    project: Option<&str>,
    api_key_id: Option<i64>,
) -> String {
    format!(
        "{}|{}|{}|{}",
        user_id,
        model,
        project.unwrap_or(""),
        api_key_id.map(|k| k.to_string()).unwrap_or_default()
    )
}
