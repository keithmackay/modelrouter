use axum::{
    extract::{Path, State},
    response::Html,
    Form,
};
use serde::Deserialize;

use super::aliases::refresh_router_aliases;
use super::audit::audit;
use super::dashboard::{DashboardError, SuperDashboardSession, render};
use crate::api::app::AppState;

pub(crate) fn he(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

// ── Form types ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateModelForm {
    pub provider: String,
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Deserialize)]
pub struct SetFailoverForm {
    /// Newline- or comma-separated fallback models in order
    pub fallbacks: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn get_models(
    State(state): State<AppState>,
    _session: SuperDashboardSession,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::models::ModelRepository;

    let models = state.db.list_models().await.map_err(|_| DashboardError::Internal)?;
    let all_failovers = state.db.list_all_failovers().await.map_err(|_| DashboardError::Internal)?;

    // Build failover map: primary_model -> Vec<fallback_model>
    let mut failover_map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for f in &all_failovers {
        failover_map.entry(f.primary_model.clone()).or_default().push(f.fallback_model.clone());
    }

    // Build context-friendly list for template
    let model_list: Vec<serde_json::Value> = models.iter().map(|m| {
        let chain = failover_map.get(m.alias.as_deref().unwrap_or(&m.name))
            .or_else(|| failover_map.get(&format!("{}/{}", m.provider, m.name)))
            .cloned()
            .unwrap_or_default();
        serde_json::json!({
            "id": m.id,
            "provider": m.provider,
            "name": m.name,
            "alias": m.alias,
            "enabled": m.enabled,
            "created_at": m.created_at,
            "disabled_reason": m.disabled_reason,
            "disabled_by": m.disabled_by,
            "disabled_at": m.disabled_at,
            "failovers": chain,
        })
    }).collect();

    // Also expose all primary keys that have failover chains configured
    let mut failover_rows: Vec<serde_json::Value> = failover_map.iter().map(|(primary, chain)| {
        serde_json::json!({ "primary": primary, "chain": chain })
    }).collect();
    failover_rows.sort_by(|a, b| {
        a["primary"].as_str().unwrap_or("").cmp(b["primary"].as_str().unwrap_or(""))
    });

    let shortcuts_fastest = state.settings.routing.shortcuts.fastest.clone();
    let shortcuts_cheapest = state.settings.routing.shortcuts.cheapest.clone();

    // Config-file aliases, flagged where a runtime alias overrides them.
    let effective = super::aliases::build_db_alias_map(&state.db).await;
    let mut config_aliases: Vec<serde_json::Value> = state
        .settings
        .routing
        .model_aliases
        .iter()
        .map(|(alias, target)| {
            serde_json::json!({
                "alias": alias,
                "target": target,
                "overridden": effective.contains_key(alias),
            })
        })
        .collect();
    config_aliases.sort_by(|a, b| {
        a["alias"].as_str().unwrap_or("").cmp(b["alias"].as_str().unwrap_or(""))
    });

    render(
        "models.html",
        minijinja::context! {
            models => minijinja::Value::from_serialize(&model_list),
            failover_rows => minijinja::Value::from_serialize(&failover_rows),
            config_aliases => minijinja::Value::from_serialize(&config_aliases),
            shortcuts_fastest => shortcuts_fastest,
            shortcuts_cheapest => shortcuts_cheapest,
        },
    )
}

pub async fn post_create_model(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Form(form): Form<CreateModelForm>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::models::ModelRepository;
    use crate::db::models::NewModel;

    let provider = form.provider.trim().to_string();
    let name = form.name.trim().to_string();
    let alias = form.alias.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from);

    if provider.is_empty() || name.is_empty() {
        return Ok(Html(
            "<div class=\"alert alert-danger\">Provider and name are required.</div>".to_string()
        ));
    }

    let model = state.db.create_model(NewModel { provider: provider.clone(), name: name.clone(), alias: alias.clone() })
        .await
        .map_err(|_| DashboardError::Internal)?;

    audit(&state.db, Some(session.0.sub), &session.0.name, "model.create",
        Some(format!("model:{}", model.id)), None,
        Some(serde_json::json!({ "provider": provider, "name": name, "alias": alias }).to_string()),
    ).await;

    // Refresh DB alias map on router
    refresh_router_aliases(&state).await;

    let alias_display = model.alias.as_deref().unwrap_or("—");
    Ok(Html(format!(
        "<tr id=\"model-row-{id}\">\
          <td>{id}</td>\
          <td>{provider}</td>\
          <td>{name}</td>\
          <td>{alias}</td>\
          <td><span class=\"tag tag-enabled\">Enabled</span></td>\
          <td>\
            <button class=\"btn btn-danger\" style=\"font-size:0.8rem;padding:0.25rem 0.5rem\" \
              hx-post=\"/admin/models/{id}/delete\" hx-target=\"#model-row-{id}\" hx-swap=\"outerHTML\">Delete</button>\
            <button class=\"btn btn-secondary\" style=\"font-size:0.8rem;padding:0.25rem 0.5rem\" \
              hx-post=\"/admin/models/{id}/disable\" hx-target=\"#model-row-{id}\" hx-swap=\"outerHTML\">Disable</button>\
          </td>\
        </tr>\
        <div id=\"model-form-message\" hx-swap-oob=\"innerHTML\">\
          <div class=\"alert\" style=\"background:#d4edda;border:1px solid #c3e6cb;color:#155724\">\
            ✓ Model <strong>{provider2}/{name2}</strong> created (id={id}).\
          </div>\
        </div>",
        id = model.id,
        provider = he(&model.provider),
        name = he(&model.name),
        alias = he(alias_display),
        provider2 = he(&model.provider),
        name2 = he(&model.name),
    )))
}

pub async fn post_disable_model(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(id): Path<i64>,
    Form(form): Form<DisableReasonForm>,
) -> Result<Html<String>, DashboardError> {
    set_model_from_dashboard(state, session, id, false, clean_reason(form.reason.as_deref())).await
}

pub async fn post_enable_model(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    set_model_from_dashboard(state, session, id, true, None).await
}

async fn set_model_from_dashboard(
    state: AppState,
    session: SuperDashboardSession,
    id: i64,
    enabled: bool,
    reason: Option<String>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::models::ModelRepository;

    let before = state.db.get_model(id).await.map_err(|_| DashboardError::Internal)?
        .ok_or_else(|| DashboardError::NotFound(format!("model {id}")))?;

    state
        .db
        .set_model_enabled_with_reason(id, enabled, reason.as_deref(), Some(&session.0.name))
        .await
        .map_err(|_| DashboardError::Internal)?;
    refresh_router_state(&state).await;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        if enabled { "model.enable" } else { "model.disable" },
        Some(format!("model:{}/{}", before.provider, before.name)),
        Some(serde_json::json!({ "enabled": before.enabled }).to_string()),
        Some(serde_json::json!({ "enabled": enabled, "reason": reason }).to_string()),
    )
    .await;

    let model = state.db.get_model(id).await.map_err(|_| DashboardError::Internal)?
        .ok_or_else(|| DashboardError::NotFound(format!("model {id}")))?;
    Ok(Html(model_row_html(&model)))
}

pub async fn post_delete_model(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::models::ModelRepository;

    state.db.delete_model(id).await.map_err(|_| DashboardError::Internal)?;
    refresh_router_aliases(&state).await;

    audit(&state.db, Some(session.0.sub), &session.0.name, "model.delete",
        Some(format!("model:{}", id)), None, None).await;

    Ok(Html(String::new())) // Remove row via hx-swap outerHTML
}

pub async fn post_set_failovers(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(primary): Path<String>,
    Form(form): Form<SetFailoverForm>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::models::ModelRepository;

    let fallbacks: Vec<String> = form.fallbacks
        .split(|c| c == ',' || c == '\n')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    state.db.set_failovers(&primary, &fallbacks).await.map_err(|_| DashboardError::Internal)?;

    audit(&state.db, Some(session.0.sub), &session.0.name, "model.failover.set",
        Some(format!("model:{}", primary)), None,
        Some(serde_json::json!({ "fallbacks": fallbacks }).to_string()),
    ).await;

    // Refresh DB failover map on router
    refresh_router_failovers(&state).await;

    let chain_display = if fallbacks.is_empty() {
        "<em>cleared</em>".to_string()
    } else {
        fallbacks.iter().map(|f| he(f)).collect::<Vec<_>>().join(" → ")
    };

    Ok(Html(format!(
        "<div class=\"alert\" style=\"background:#d4edda;border:1px solid #c3e6cb;color:#155724;margin-top:0.5rem\">\
          ✓ Failover chain for <strong>{}</strong> saved: {}\
        </div>",
        he(&primary),
        chain_display,
    )))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn model_row_html(m: &crate::db::models::Model) -> String {
    let id = m.id;
    let status_tag = if m.enabled {
        "<span class=\"tag tag-enabled\">Enabled</span>".to_string()
    } else {
        format!(
            "<span class=\"tag tag-disabled\">Disabled</span><br>\
             <small style=\"color:#666\">{reason} — {by}{at}</small>",
            reason = he(m.disabled_reason.as_deref().unwrap_or("no reason recorded")),
            by = he(m.disabled_by.as_deref().unwrap_or("unknown")),
            at = m.disabled_at.as_deref().map(|a| format!(", {}", he(a))).unwrap_or_default(),
        )
    };
    let toggle_btn = if m.enabled {
        format!(
            "<input type=\"text\" name=\"reason\" placeholder=\"reason\" id=\"model-reason-{id}\" \
              style=\"padding:0.2rem 0.4rem;border:1px solid #ccc;border-radius:4px;width:150px\">\
             <button class=\"btn btn-secondary\" style=\"font-size:0.8rem;padding:0.25rem 0.5rem\" \
              hx-post=\"/admin/models/{id}/disable\" hx-include=\"#model-reason-{id}\" \
              hx-target=\"#model-row-{id}\" hx-swap=\"outerHTML\">Disable</button>"
        )
    } else {
        format!(
            "<button class=\"btn btn-success\" style=\"font-size:0.8rem;padding:0.25rem 0.5rem\" \
              hx-post=\"/admin/models/{id}/enable\" hx-target=\"#model-row-{id}\" hx-swap=\"outerHTML\">Enable</button>"
        )
    };
    format!(
        "<tr id=\"model-row-{id}\">\
          <td>{id}</td>\
          <td>{provider}</td>\
          <td>{name}</td>\
          <td>{alias}</td>\
          <td>{status}</td>\
          <td>\
            <button class=\"btn btn-danger\" style=\"font-size:0.8rem;padding:0.25rem 0.5rem\" \
              hx-post=\"/admin/models/{id}/delete\" hx-target=\"#model-row-{id}\" hx-swap=\"outerHTML\" \
              hx-confirm=\"Delete this model?\">Delete</button>\
            {toggle}\
          </td>\
        </tr>",
        id = id,
        provider = he(&m.provider),
        name = he(&m.name),
        alias = he(m.alias.as_deref().unwrap_or("—")),
        status = status_tag,
        toggle = toggle_btn,
    )
}

/// Reload DB failover chains into the live FallbackChain.
async fn refresh_router_failovers(state: &AppState) {
    use crate::db::repositories::models::ModelRepository;

    if let Ok(rows) = state.db.list_all_failovers().await {
        let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for r in rows {
            map.entry(r.primary_model).or_default().push(r.fallback_model);
        }
        state.fallback.update_db_chains(map);
    }
}

// ── Operator disable / enable (issue #5) ──────────────────────────────────────
//
// An operator disable is deliberately *sticky*: unlike a circuit-breaker trip it
// never auto-recovers, it is persisted so it survives a restart, and only an
// explicit enable clears it. See `crate::router::availability` for the full
// comparison and for how disabled entities are excluded from routing.

use axum::response::IntoResponse;
use crate::api::admin::auth::{AdminSession, SuperAdminSession};
use crate::api::error::ApiError;
use super::aliases::refresh_router_state;

#[derive(Deserialize)]
pub struct SetEnabledRequest {
    pub enabled: bool,
    /// Required in spirit when disabling — recorded verbatim and shown to callers.
    pub reason: Option<String>,
}

#[derive(Deserialize)]
pub struct DisableReasonForm {
    pub reason: Option<String>,
}

fn clean_reason(reason: Option<&str>) -> Option<String> {
    reason.map(str::trim).filter(|r| !r.is_empty()).map(String::from)
}

/// GET /admin/api/models — models with their disable metadata.
pub async fn list_models_api(
    State(state): State<AppState>,
    _session: AdminSession,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::models::ModelRepository;

    let models = state.db.list_models().await.map_err(|_| ApiError::Internal)?;
    Ok(axum::Json(serde_json::json!({ "models": models })))
}

/// PATCH /admin/api/models/:id/enabled
pub async fn set_model_enabled_api(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Path(id): Path<i64>,
    axum::Json(body): axum::Json<SetEnabledRequest>,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::models::ModelRepository;

    let before = state
        .db
        .get_model(id)
        .await
        .map_err(|_| ApiError::Internal)?
        .ok_or_else(|| ApiError::InvalidRequest(format!("no such model: {id}")))?;

    let reason = clean_reason(body.reason.as_deref());
    state
        .db
        .set_model_enabled_with_reason(id, body.enabled, reason.as_deref(), Some(&session.0.name))
        .await
        .map_err(|_| ApiError::Internal)?;

    refresh_router_state(&state).await;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        if body.enabled { "model.enable" } else { "model.disable" },
        Some(format!("model:{}/{}", before.provider, before.name)),
        Some(serde_json::json!({ "enabled": before.enabled }).to_string()),
        Some(serde_json::json!({ "enabled": body.enabled, "reason": reason }).to_string()),
    )
    .await;

    let after = state
        .db
        .get_model(id)
        .await
        .map_err(|_| ApiError::Internal)?
        .ok_or(ApiError::Internal)?;
    Ok(axum::Json(after))
}

/// GET /admin/api/providers — configured providers with their enable state.
pub async fn list_providers_api(
    State(state): State<AppState>,
    _session: AdminSession,
) -> Result<impl IntoResponse, ApiError> {
    Ok(axum::Json(serde_json::json!({
        "providers": provider_views(&state).await.map_err(|_| ApiError::Internal)?,
    })))
}

/// PATCH /admin/api/providers/:provider/enabled
pub async fn set_provider_enabled_api(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Path(provider): Path<String>,
    axum::Json(body): axum::Json<SetEnabledRequest>,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::models::ModelRepository;

    // Only providers this router actually knows about can be toggled, so a typo
    // cannot create a phantom "disabled" entry that silently does nothing.
    if !state.settings.providers.contains_key(&provider) {
        return Err(ApiError::InvalidRequest(format!(
            "unknown provider: {provider}"
        )));
    }

    let reason = clean_reason(body.reason.as_deref());
    state
        .db
        .set_provider_enabled(&provider, body.enabled, reason.as_deref(), Some(&session.0.name))
        .await
        .map_err(|_| ApiError::Internal)?;

    refresh_router_state(&state).await;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        if body.enabled { "provider.enable" } else { "provider.disable" },
        Some(format!("provider:{provider}")),
        None,
        Some(serde_json::json!({ "enabled": body.enabled, "reason": reason }).to_string()),
    )
    .await;

    let state_row = state
        .db
        .get_provider_state(&provider)
        .await
        .map_err(|_| ApiError::Internal)?;
    Ok(axum::Json(serde_json::json!({
        "provider": provider,
        "enabled": body.enabled,
        "state": state_row,
    })))
}

/// Configured providers joined with any persisted disable state.
async fn provider_views(state: &AppState) -> anyhow::Result<Vec<serde_json::Value>> {
    use crate::db::repositories::models::ModelRepository;

    let states = state.db.list_provider_states().await?;
    let mut names: Vec<String> = state.settings.providers.keys().cloned().collect();
    for s in &states {
        if !names.contains(&s.provider) {
            names.push(s.provider.clone());
        }
    }
    names.sort();

    Ok(names
        .into_iter()
        .map(|name| {
            let row = states.iter().find(|s| s.provider == name);
            serde_json::json!({
                "provider": name,
                "enabled": row.map(|r| r.enabled).unwrap_or(true),
                "disabled_reason": row.and_then(|r| r.disabled_reason.clone()),
                "disabled_by": row.and_then(|r| r.disabled_by.clone()),
                "disabled_at": row.and_then(|r| r.disabled_at.clone()),
            })
        })
        .collect())
}

/// POST /admin/providers/:provider/disable — dashboard toggle.
pub async fn post_disable_provider(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(provider): Path<String>,
    Form(form): Form<DisableReasonForm>,
) -> Result<Html<String>, DashboardError> {
    set_provider_from_dashboard(state, session, provider, false, clean_reason(form.reason.as_deref())).await
}

/// POST /admin/providers/:provider/enable — dashboard toggle.
pub async fn post_enable_provider(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(provider): Path<String>,
) -> Result<Html<String>, DashboardError> {
    set_provider_from_dashboard(state, session, provider, true, None).await
}

async fn set_provider_from_dashboard(
    state: AppState,
    session: SuperDashboardSession,
    provider: String,
    enabled: bool,
    reason: Option<String>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::models::ModelRepository;

    if !state.settings.providers.contains_key(&provider) {
        return Err(DashboardError::NotFound(format!("provider {provider}")));
    }

    state
        .db
        .set_provider_enabled(&provider, enabled, reason.as_deref(), Some(&session.0.name))
        .await
        .map_err(|_| DashboardError::Internal)?;

    refresh_router_state(&state).await;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        if enabled { "provider.enable" } else { "provider.disable" },
        Some(format!("provider:{provider}")),
        None,
        Some(serde_json::json!({ "enabled": enabled, "reason": reason }).to_string()),
    )
    .await;

    let row = state
        .db
        .get_provider_state(&provider)
        .await
        .map_err(|_| DashboardError::Internal)?;
    Ok(Html(provider_row_html(
        &provider,
        enabled,
        row.as_ref().and_then(|r| r.disabled_reason.as_deref()),
        row.as_ref().and_then(|r| r.disabled_by.as_deref()),
        row.as_ref().and_then(|r| r.disabled_at.as_deref()),
    )))
}

pub(crate) fn provider_row_html(
    provider: &str,
    enabled: bool,
    reason: Option<&str>,
    by: Option<&str>,
    at: Option<&str>,
) -> String {
    let id = provider
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let status = if enabled {
        "<span class=\"tag tag-enabled\">Enabled</span>".to_string()
    } else {
        format!(
            "<span class=\"tag tag-disabled\">Disabled</span><br>\
             <small style=\"color:#666\">{reason} — {by}{at}</small>",
            reason = he(reason.unwrap_or("no reason recorded")),
            by = he(by.unwrap_or("unknown")),
            at = at.map(|a| format!(", {}", he(a))).unwrap_or_default(),
        )
    };
    let action = if enabled {
        format!(
            "<input type=\"text\" name=\"reason\" placeholder=\"reason\" \
               style=\"padding:0.2rem 0.4rem;border:1px solid #ccc;border-radius:4px;width:150px\" \
               id=\"provider-reason-{id}\">\
             <button class=\"btn btn-secondary\" style=\"font-size:0.8rem;padding:0.25rem 0.5rem\" \
               hx-post=\"/admin/providers/{purl}/disable\" hx-include=\"#provider-reason-{id}\" \
               hx-target=\"#provider-row-{id}\" hx-swap=\"outerHTML\">Disable</button>",
            id = id,
            purl = urlencoding::encode(provider),
        )
    } else {
        format!(
            "<button class=\"btn btn-success\" style=\"font-size:0.8rem;padding:0.25rem 0.5rem\" \
               hx-post=\"/admin/providers/{purl}/enable\" \
               hx-target=\"#provider-row-{id}\" hx-swap=\"outerHTML\">Enable</button>",
            id = id,
            purl = urlencoding::encode(provider),
        )
    };
    format!(
        "<tr id=\"provider-row-{id}\"><td><code>{name}</code></td><td>{status}</td><td>{action}</td></tr>",
        id = id,
        name = he(provider),
        status = status,
        action = action,
    )
}

/// GET /admin/providers/rows — htmx fragment for the provider table body.
pub async fn get_provider_rows(
    State(state): State<AppState>,
    _session: SuperDashboardSession,
) -> Result<Html<String>, DashboardError> {
    let views = provider_views(&state).await.map_err(|_| DashboardError::Internal)?;
    if views.is_empty() {
        return Ok(Html(
            "<tr><td colspan=\"3\" style=\"color:#999;font-style:italic;\">\
             No providers configured.</td></tr>"
                .to_string(),
        ));
    }
    Ok(Html(
        views
            .iter()
            .map(|v| {
                provider_row_html(
                    v["provider"].as_str().unwrap_or(""),
                    v["enabled"].as_bool().unwrap_or(true),
                    v["disabled_reason"].as_str(),
                    v["disabled_by"].as_str(),
                    v["disabled_at"].as_str(),
                )
            })
            .collect::<String>(),
    ))
}


// ── Available-models catalog endpoint (issue #34) ────────────────────────────

/// TTL cache for the aggregated catalog: provider catalog calls cost quota,
/// and the mapping UI refetches freely. 15 minutes, bypassed by ?refresh=true.
static CATALOG_CACHE: std::sync::OnceLock<tokio::sync::Mutex<Option<(std::time::Instant, serde_json::Value)>>> =
    std::sync::OnceLock::new();
const CATALOG_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

#[derive(serde::Deserialize)]
pub struct AvailableModelsQuery {
    #[serde(default)]
    pub refresh: bool,
}

/// Shared catalog cache used by both the models API and alias validation.
/// Returns the aggregated catalog, using the cache unless `refresh` is true.
pub(crate) async fn cached_catalog(
    state: &AppState,
    refresh: bool,
) -> serde_json::Value {
    let cache = CATALOG_CACHE.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut guard = cache.lock().await;
    if !refresh {
        if let Some((at, value)) = guard.as_ref() {
            if at.elapsed() < CATALOG_TTL {
                return value.clone();
            }
        }
    }
    let providers = crate::providers::catalog_registry::aggregate_catalogs(
        &state.live_settings.load().providers,
    )
    .await;
    let value = serde_json::json!({
        "providers": providers,
        "ttl_seconds": CATALOG_TTL.as_secs(),
    });
    *guard = Some((std::time::Instant::now(), value.clone()));
    value
}

/// GET /admin/api/models/available — what each configured provider's catalog
/// actually offers, per-provider degraded, TTL-cached.
pub async fn get_available_models(
    State(state): State<AppState>,
    _session: AdminSession,
    axum::extract::Query(q): axum::extract::Query<AvailableModelsQuery>,
) -> Result<axum::Json<serde_json::Value>, ApiError> {
    let value = cached_catalog(&state, q.refresh).await;
    Ok(axum::Json(value))
}
