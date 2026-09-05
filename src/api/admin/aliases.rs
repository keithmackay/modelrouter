//! Runtime administration of model aliases (issue #9).
//!
//! Alias precedence, highest first:
//!   1. `model_aliases` rows (managed here — API / UI / CLI)
//!   2. `models.alias` on an *enabled* registered model row
//!   3. config-file `routing.model_aliases`
//!
//! (1) and (2) are merged into the router's DB alias map, which
//! [`crate::router::engine::RequestRouter::resolve`] consults before config.
//! Every write refreshes that live map, so changes take effect for the next
//! request without a restart, and is recorded in the audit log.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    response::{Html, IntoResponse},
    Form, Json,
};
use serde::Deserialize;

use super::audit::audit;
use super::dashboard::{DashboardError, SuperDashboardSession};
use crate::api::{
    admin::auth::{AdminSession, SuperAdminSession},
    app::{AppState, DatabaseProvider},
    error::ApiError,
};
use crate::db::models::NewModelAlias;

/// Depth cap mirroring `RequestRouter::resolve`, used for write-time cycle detection.
const MAX_ALIAS_DEPTH: usize = 10;

// ── Alias map construction ────────────────────────────────────────────────────

/// Build the effective DB alias map: enabled model-row aliases, overridden by
/// explicitly managed `model_aliases` rows.
pub async fn build_db_alias_map(db: &Arc<dyn DatabaseProvider>) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    if let Ok(models) = db.list_models().await {
        for m in models.iter().filter(|m| m.enabled) {
            if let Some(alias) = m.alias.as_ref() {
                map.insert(alias.clone(), format!("{}/{}", m.provider, m.name));
            }
        }
    }
    if let Ok(rows) = db.list_aliases().await {
        for row in rows {
            map.insert(row.alias, row.target);
        }
    }
    map
}

/// Reload the DB alias map into the live router. Call after any alias or model write.
pub async fn refresh_router_aliases(state: &AppState) {
    let map = build_db_alias_map(&state.db).await;
    state.router.update_db_aliases(map);
}

/// Build the operator-disable snapshot from the database (issue #5).
pub async fn build_availability_map(
    db: &Arc<dyn DatabaseProvider>,
) -> crate::router::availability::AvailabilityMap {
    use crate::router::availability::DisableInfo;

    let mut models: HashMap<String, DisableInfo> = HashMap::new();
    if let Ok(rows) = db.list_models().await {
        for m in rows.iter().filter(|m| !m.enabled) {
            models.insert(
                format!("{}/{}", m.provider, m.name),
                DisableInfo {
                    reason: m.disabled_reason.clone(),
                    by: m.disabled_by.clone(),
                    at: m.disabled_at.clone(),
                },
            );
        }
    }
    let mut providers: HashMap<String, DisableInfo> = HashMap::new();
    if let Ok(rows) = db.list_provider_states().await {
        for p in rows.iter().filter(|p| !p.enabled) {
            providers.insert(
                p.provider.clone(),
                DisableInfo {
                    reason: p.disabled_reason.clone(),
                    by: p.disabled_by.clone(),
                    at: p.disabled_at.clone(),
                },
            );
        }
    }
    crate::router::availability::AvailabilityMap::new(models, providers)
}

/// Reload both the alias map and the disable snapshot into the live router.
/// Called after any model, provider or alias write so changes need no restart.
pub async fn refresh_router_state(state: &AppState) {
    refresh_router_aliases(state).await;
    state
        .router
        .update_availability(build_availability_map(&state.db).await);
}

/// Returns `true` if adding `alias -> target` to `map` would create a resolution cycle.
fn would_cycle(map: &HashMap<String, String>, alias: &str, target: &str) -> bool {
    let mut current = target.to_string();
    for _ in 0..MAX_ALIAS_DEPTH {
        if current == alias {
            return true;
        }
        match map.get(&current) {
            Some(next) => current = next.clone(),
            None => return false,
        }
    }
    // Ran out of depth without terminating — treat as a cycle.
    true
}

/// Fetch the catalog and extract all available model IDs (provider/name format).
/// Returns None if the catalog is unavailable (fetch error), or Some(set) when available.
async fn fetch_available_model_ids(state: &AppState) -> Option<std::collections::HashSet<String>> {
    let catalog_result = crate::providers::catalog_registry::aggregate_catalogs(
        &state.live_settings.load().providers,
    )
    .await;

    let providers = match catalog_result.as_object() {
        Some(obj) => obj,
        None => return None,
    };

    let mut model_ids = std::collections::HashSet::new();
    for (_provider_name, provider_data) in providers.iter() {
        if let Some(models) = provider_data.get("models").and_then(|m| m.as_array()) {
            for model in models {
                // CatalogModel has 'provider' and 'name' fields
                if let (Some(provider), Some(name)) = (
                    model.get("provider").and_then(|p| p.as_str()),
                    model.get("name").and_then(|n| n.as_str()),
                ) {
                    model_ids.insert(format!("{}/{}", provider, name));
                }
            }
        }
    }

    if model_ids.is_empty() {
        None
    } else {
        Some(model_ids)
    }
}

/// Shared validation for an alias write. Returns the trimmed (alias, target).
async fn validate_alias(
    state: &AppState,
    alias: &str,
    target: &str,
) -> Result<(String, String), String> {
    let alias = alias.trim().to_string();
    let target = target.trim().to_string();

    if alias.is_empty() || target.is_empty() {
        return Err("alias and target are required".to_string());
    }
    if alias.starts_with(':') {
        return Err("aliases may not start with ':' — that prefix is reserved for routing shortcuts".to_string());
    }
    if alias == target {
        return Err(format!("alias '{alias}' cannot point at itself"));
    }

    let mut map = build_db_alias_map(&state.db).await;
    map.remove(&alias);
    for (k, v) in state.settings.routing.model_aliases.iter() {
        map.entry(k.clone()).or_insert_with(|| v.clone());
    }
    if would_cycle(&map, &alias, &target) {
        return Err(format!(
            "alias '{alias}' -> '{target}' would create an alias cycle"
        ));
    }

    // Validate target against the catalog when available (issue #35).
    match fetch_available_model_ids(state).await {
        Some(available) => {
            if !available.contains(&target) {
                return Err(format!(
                    "model '{}' not found in provider catalogs — validation can be bypassed when catalog is unavailable",
                    target
                ));
            }
        }
        None => {
            // Catalog unavailable — degrade gracefully, accept the write with a warning.
            tracing::warn!(
                alias = %alias,
                target = %target,
                "alias write accepted without catalog validation — provider catalog unavailable"
            );
        }
    }

    Ok((alias, target))
}

// ── JSON admin API ────────────────────────────────────────────────────────────

/// GET /admin/api/aliases
pub async fn list_aliases_api(
    State(state): State<AppState>,
    _session: AdminSession,
) -> Result<impl IntoResponse, ApiError> {
    let rows = state.db.list_aliases().await.map_err(|_| ApiError::Internal)?;
    let effective = build_db_alias_map(&state.db).await;
    Ok(Json(serde_json::json!({
        "aliases": rows,
        "effective": effective,
        "config_aliases": state.settings.routing.model_aliases,
    })))
}

#[derive(Deserialize)]
pub struct UpsertAliasRequest {
    pub target: String,
}

/// PUT /admin/api/aliases/:alias — create or update.
pub async fn upsert_alias_api(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Path(alias): Path<String>,
    Json(body): Json<UpsertAliasRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let (alias, target) = validate_alias(&state, &alias, &body.target)
        .await
        .map_err(ApiError::InvalidRequest)?;

    let before = state
        .db
        .get_alias(&alias)
        .await
        .map_err(|_| ApiError::Internal)?;

    let row = state
        .db
        .upsert_alias(NewModelAlias {
            alias: alias.clone(),
            target: target.clone(),
            created_by: Some(session.0.name.clone()),
        })
        .await
        .map_err(|_| ApiError::Internal)?;

    refresh_router_aliases(&state).await;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        if before.is_some() { "alias.update" } else { "alias.create" },
        Some(format!("alias:{alias}")),
        before.map(|b| serde_json::json!({ "target": b.target }).to_string()),
        Some(serde_json::json!({ "target": target }).to_string()),
    )
    .await;

    Ok(Json(row))
}

/// DELETE /admin/api/aliases/:alias
pub async fn delete_alias_api(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Path(alias): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let before = state
        .db
        .get_alias(&alias)
        .await
        .map_err(|_| ApiError::Internal)?;
    let removed = state
        .db
        .delete_alias(&alias)
        .await
        .map_err(|_| ApiError::Internal)?;

    if !removed {
        return Err(ApiError::InvalidRequest(format!("no such alias: {alias}")));
    }

    refresh_router_aliases(&state).await;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "alias.delete",
        Some(format!("alias:{alias}")),
        before.map(|b| serde_json::json!({ "target": b.target }).to_string()),
        None,
    )
    .await;

    Ok(Json(serde_json::json!({ "ok": true, "alias": alias })))
}

// ── Dashboard (htmx) ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AliasForm {
    pub alias: String,
    pub target: String,
}

/// POST /admin/aliases — create or update from the models page.
pub async fn post_set_alias(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Form(form): Form<AliasForm>,
) -> Result<Html<String>, DashboardError> {
    let (alias, target) = match validate_alias(&state, &form.alias, &form.target).await {
        Ok(v) => v,
        Err(msg) => {
            return Ok(Html(format!(
                "<div class=\"alert alert-danger\">{}</div>",
                super::models::he(&msg)
            )))
        }
    };

    let before = state.db.get_alias(&alias).await.map_err(|_| DashboardError::Internal)?;
    state
        .db
        .upsert_alias(NewModelAlias {
            alias: alias.clone(),
            target: target.clone(),
            created_by: Some(session.0.name.clone()),
        })
        .await
        .map_err(|_| DashboardError::Internal)?;

    refresh_router_aliases(&state).await;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        if before.is_some() { "alias.update" } else { "alias.create" },
        Some(format!("alias:{alias}")),
        before.map(|b| serde_json::json!({ "target": b.target }).to_string()),
        Some(serde_json::json!({ "target": target }).to_string()),
    )
    .await;

    Ok(Html(format!(
        "<div class=\"alert\" style=\"background:#d4edda;border:1px solid #c3e6cb;color:#155724\">\
           ✓ Alias <strong>{}</strong> → <code>{}</code> saved and live.\
         </div>\
         <div hx-get=\"/admin/aliases/rows\" hx-target=\"#aliases-tbody\" hx-swap=\"innerHTML\" hx-trigger=\"load\"></div>",
        super::models::he(&alias),
        super::models::he(&target),
    )))
}

/// POST /admin/aliases/:alias/delete
pub async fn post_delete_alias(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(alias): Path<String>,
) -> Result<Html<String>, DashboardError> {
    let before = state.db.get_alias(&alias).await.map_err(|_| DashboardError::Internal)?;
    state.db.delete_alias(&alias).await.map_err(|_| DashboardError::Internal)?;
    refresh_router_aliases(&state).await;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "alias.delete",
        Some(format!("alias:{alias}")),
        before.map(|b| serde_json::json!({ "target": b.target }).to_string()),
        None,
    )
    .await;

    Ok(Html(String::new()))
}

/// GET /admin/aliases/rows — htmx fragment for the alias table body.
pub async fn get_alias_rows(
    State(state): State<AppState>,
    _session: SuperDashboardSession,
) -> Result<Html<String>, DashboardError> {
    let rows = state.db.list_aliases().await.map_err(|_| DashboardError::Internal)?;
    Ok(Html(render_alias_rows(&rows)))
}

pub fn render_alias_rows(rows: &[crate::db::models::ModelAlias]) -> String {
    use super::models::he;

    if rows.is_empty() {
        return "<tr><td colspan=\"5\" style=\"color:#999;font-style:italic;\">\
                No runtime aliases defined.</td></tr>"
            .to_string();
    }
    rows.iter()
        .map(|r| {
            let id = sanitize_id(&r.alias);
            format!(
                "<tr id=\"alias-row-{id}\">\
                   <td><code>{alias}</code></td>\
                   <td><code>{target}</code></td>\
                   <td>{by}</td>\
                   <td>{updated}</td>\
                   <td>\
                     <button class=\"btn btn-secondary\" style=\"font-size:0.8rem;padding:0.25rem 0.5rem\" \
                       onclick='editAlias(\"{alias_js}\", \"{target_js}\")'>Edit</button>\
                     <button class=\"btn btn-danger\" style=\"font-size:0.8rem;padding:0.25rem 0.5rem\" \
                       hx-post=\"/admin/aliases/{alias_url}/delete\" hx-target=\"#alias-row-{id}\" \
                       hx-swap=\"outerHTML\" hx-confirm=\"Delete this alias?\">Delete</button>\
                   </td>\
                 </tr>",
                id = id,
                alias = he(&r.alias),
                target = he(&r.target),
                by = he(r.created_by.as_deref().unwrap_or("—")),
                updated = he(&r.updated_at),
                alias_js = he(&r.alias),
                target_js = he(&r.target),
                alias_url = urlencoding::encode(&r.alias),
            )
        })
        .collect()
}

/// DOM-safe id fragment for an alias (aliases may contain '/' and other chars).
fn sanitize_id(alias: &str) -> String {
    alias
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn direct_self_reference_is_a_cycle() {
        assert!(would_cycle(&HashMap::new(), "deep", "deep"));
    }

    #[test]
    fn indirect_cycle_is_detected() {
        // existing: b -> c, c -> a. Adding a -> b closes the loop.
        let m = map(&[("b", "c"), ("c", "a")]);
        assert!(would_cycle(&m, "a", "b"));
    }

    #[test]
    fn acyclic_chain_is_allowed() {
        let m = map(&[("b", "openai/gpt-5")]);
        assert!(!would_cycle(&m, "a", "b"));
    }

    #[test]
    fn overlong_chain_is_rejected() {
        let mut m = HashMap::new();
        for i in 0..MAX_ALIAS_DEPTH + 2 {
            m.insert(format!("a{i}"), format!("a{}", i + 1));
        }
        assert!(would_cycle(&m, "root", "a0"));
    }

    #[test]
    fn sanitize_id_strips_unsafe_chars() {
        assert_eq!(sanitize_id("openai/gpt-5"), "openai-gpt-5");
    }
}
