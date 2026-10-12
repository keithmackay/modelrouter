//! Admin view of model capabilities learned from provider rejections.
//!
//! `GET` lists every learned entry with the time it was learned, the error
//! that taught it and how many requests it has changed since start. `DELETE`
//! clears one (both the live set and the stored row) once the provider has
//! restored support; the next rejection, if any, learns it again.

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};

use super::audit::audit;
use crate::api::{
    admin::auth::{AdminSession, SuperAdminSession},
    app::AppState,
    error::ApiError,
};
use crate::router::learned_capabilities::learned_key;

/// Load the stored learned capabilities into the live router (startup).
pub async fn load_learned_capabilities(
    router: &crate::router::engine::RequestRouter,
    db: &dyn crate::api::app::DatabaseProvider,
) {
    match db.list_learned_capabilities().await {
        Ok(rows) => {
            if !rows.is_empty() {
                tracing::info!(count = rows.len(), "loaded learned model capabilities");
            }
            router.learned_capabilities().replace_all(rows);
        }
        Err(e) => tracing::error!(error = %e, "failed to load learned model capabilities"),
    }
}

/// GET /admin/api/model-capabilities/learned
pub async fn list_learned_capabilities_api(
    State(state): State<AppState>,
    _session: AdminSession,
) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(serde_json::json!({
        "learned": state.router.learned_capabilities().snapshot(),
    })))
}

/// DELETE /admin/api/model-capabilities/learned/:model
pub async fn delete_learned_capability_api(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Path(model): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let key = learned_key(&model);
    let stored = state
        .db
        .delete_learned_capability(&key)
        .await
        .map_err(|_| ApiError::Internal)?;
    let live = state.router.learned_capabilities().remove(&key);
    if !stored && !live {
        return Err(ApiError::InvalidRequest(format!("no learned capability for model: {key}")));
    }
    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "learned_capability.delete",
        Some(format!("model:{key}")),
        None,
        None,
    )
    .await;
    Ok(Json(serde_json::json!({ "deleted": key })))
}
