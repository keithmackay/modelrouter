use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect},
    Form, Json,
};
use serde::{Deserialize, Serialize};

use super::dashboard::{DashboardError, DashboardSession, SuperDashboardSession, render};
use crate::api::app::AppState;
use crate::db::repositories::webhook_callbacks::{NewWebhookCallback, WebhookCallback, WebhookCallbackRepository};

fn he(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

// ── REST API types ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateWebhookJson {
    pub name: String,
    pub url: String,
    pub events: Option<String>,
    pub secret_header_name: Option<String>,
    pub secret_header_value: Option<String>,
}

#[derive(Serialize)]
pub struct WebhookResponse {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub events: String,
    pub secret_header_name: Option<String>,
    pub enabled: bool,
    pub created_at: String,
}

impl From<WebhookCallback> for WebhookResponse {
    fn from(w: WebhookCallback) -> Self {
        WebhookResponse {
            id: w.id,
            name: w.name,
            url: w.url,
            events: w.events,
            secret_header_name: w.secret_header_name,
            enabled: w.enabled,
            created_at: w.created_at,
        }
    }
}

// ── REST API handlers ──────────────────────────────────────────────────────────

pub async fn list_webhooks_api(
    State(state): State<AppState>,
    _session: DashboardSession,
) -> impl IntoResponse {
    match state.db.list_webhooks().await {
        Ok(rows) => {
            let resp: Vec<WebhookResponse> = rows.into_iter().map(WebhookResponse::from).collect();
            Json(resp).into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response(),
    }
}

pub async fn create_webhook_api(
    State(state): State<AppState>,
    _session: SuperDashboardSession,
    Json(body): Json<CreateWebhookJson>,
) -> impl IntoResponse {
    let new = NewWebhookCallback {
        name: body.name,
        url: body.url,
        events: body.events.unwrap_or_else(|| r#"["completion"]"#.to_string()),
        secret_header_name: body.secret_header_name,
        secret_header_value: body.secret_header_value,
    };
    match state.db.create_webhook(new).await {
        Ok(w) => (StatusCode::CREATED, Json(WebhookResponse::from(w))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response(),
    }
}

pub async fn delete_webhook_api(
    State(state): State<AppState>,
    _session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match state.db.delete_webhook(id).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response(),
    }
}

pub async fn enable_webhook_api(
    State(state): State<AppState>,
    _session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match state.db.set_webhook_enabled(id, true).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response(),
    }
}

pub async fn disable_webhook_api(
    State(state): State<AppState>,
    _session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match state.db.set_webhook_enabled(id, false).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response(),
    }
}

// ── Dashboard form types ───────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateWebhookForm {
    pub name: String,
    pub url: String,
    pub events: Option<String>,
    pub secret_header_name: Option<String>,
    pub secret_header_value: Option<String>,
}

// ── Dashboard page handlers ────────────────────────────────────────────────────

pub async fn get_webhooks_page(
    State(state): State<AppState>,
    _session: DashboardSession,
) -> Result<Html<String>, DashboardError> {
    let webhooks = state.db.list_webhooks().await.map_err(|_| DashboardError::Internal)?;
    let webhook_list: Vec<serde_json::Value> = webhooks.iter().map(|w| {
        serde_json::json!({
            "id": w.id,
            "name": w.name,
            "url": w.url,
            "events": w.events,
            "secret_header_name": w.secret_header_name,
            "enabled": w.enabled,
            "created_at": w.created_at,
        })
    }).collect();

    render(
        "webhooks.html",
        minijinja::context! {
            webhooks => minijinja::Value::from_serialize(&webhook_list),
        },
    )
}

pub async fn post_create_webhook_page(
    State(state): State<AppState>,
    _session: DashboardSession,
    Form(form): Form<CreateWebhookForm>,
) -> Result<impl IntoResponse, DashboardError> {
    let name = form.name.trim().to_string();
    let url = form.url.trim().to_string();
    let events = form.events
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            // Accept comma-separated event names or raw JSON
            if s.starts_with('[') {
                s.to_string()
            } else {
                let parts: Vec<String> = s.split(',').map(|e| format!("\"{}\"", e.trim())).collect();
                format!("[{}]", parts.join(","))
            }
        })
        .unwrap_or_else(|| r#"["completion"]"#.to_string());

    if name.is_empty() || url.is_empty() {
        return Ok(Redirect::to("/admin/webhooks").into_response());
    }

    let _ = state.db.create_webhook(NewWebhookCallback {
        name,
        url,
        events,
        secret_header_name: form.secret_header_name.filter(|s| !s.trim().is_empty()),
        secret_header_value: form.secret_header_value.filter(|s| !s.trim().is_empty()),
    }).await;

    Ok(Redirect::to("/admin/webhooks").into_response())
}

pub async fn post_delete_webhook_page(
    State(state): State<AppState>,
    _session: DashboardSession,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let _ = state.db.delete_webhook(id).await;
    Redirect::to("/admin/webhooks")
}

pub async fn post_enable_webhook_page_dash(
    State(state): State<AppState>,
    _session: DashboardSession,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let _ = state.db.set_webhook_enabled(id, true).await;
    Redirect::to("/admin/webhooks")
}

pub async fn post_disable_webhook_page_dash(
    State(state): State<AppState>,
    _session: DashboardSession,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let _ = state.db.set_webhook_enabled(id, false).await;
    Redirect::to("/admin/webhooks")
}
