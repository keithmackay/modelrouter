use axum::{
    extract::{Path, State},
    response::Html,
    Form,
};
use serde::Deserialize;

use super::audit::audit;
use super::dashboard::{DashboardError, SuperDashboardSession, render};
use crate::api::app::AppState;

fn he(s: &str) -> String {
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

    render(
        "models.html",
        minijinja::context! {
            models => minijinja::Value::from_serialize(&model_list),
            failover_rows => minijinja::Value::from_serialize(&failover_rows),
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
    _session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::models::ModelRepository;

    state.db.set_model_enabled(id, false).await.map_err(|_| DashboardError::Internal)?;
    refresh_router_aliases(&state).await;

    let model = state.db.get_model(id).await.map_err(|_| DashboardError::Internal)?
        .ok_or_else(|| DashboardError::NotFound(format!("model {id}")))?;
    Ok(Html(model_row_html(&model)))
}

pub async fn post_enable_model(
    State(state): State<AppState>,
    _session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::models::ModelRepository;

    state.db.set_model_enabled(id, true).await.map_err(|_| DashboardError::Internal)?;
    refresh_router_aliases(&state).await;

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
        "<span class=\"tag tag-enabled\">Enabled</span>"
    } else {
        "<span class=\"tag tag-disabled\">Disabled</span>"
    };
    let toggle_btn = if m.enabled {
        format!(
            "<button class=\"btn btn-secondary\" style=\"font-size:0.8rem;padding:0.25rem 0.5rem\" \
              hx-post=\"/admin/models/{id}/disable\" hx-target=\"#model-row-{id}\" hx-swap=\"outerHTML\">Disable</button>"
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

/// Reload DB aliases into the live router alias map.
async fn refresh_router_aliases(state: &AppState) {
    use crate::db::repositories::models::ModelRepository;

    if let Ok(models) = state.db.list_models().await {
        let map: std::collections::HashMap<String, String> = models
            .iter()
            .filter(|m| m.enabled)
            .filter_map(|m| {
                m.alias.as_ref().map(|alias| {
                    (alias.clone(), format!("{}/{}", m.provider, m.name))
                })
            })
            .collect();
        state.router.update_db_aliases(map);
    }
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
