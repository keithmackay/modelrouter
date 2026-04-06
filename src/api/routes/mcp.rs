use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::{
    api::{app::AppState, auth::AuthenticatedUser},
    db::{models::NewMcpServer, repositories::mcp_servers::McpServerRepository},
};

#[derive(Deserialize)]
pub struct CreateMcpServerRequest {
    pub name: String,
    pub url: String,
    pub description: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateMcpServerRequest {
    pub name: Option<String>,
    pub url: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
}

pub async fn list_mcp_servers(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let servers = state.db.list_mcp_servers().await.map_err(|e| {
        tracing::error!(error = %e, "Failed to list MCP servers");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "internal error" })),
        )
    })?;
    Ok(Json(serde_json::json!({ "servers": servers })))
}

pub async fn create_mcp_server(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
    Json(req): Json<CreateMcpServerRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    let server = state.db.create_mcp_server(NewMcpServer {
        name: req.name,
        url: req.url,
        description: req.description,
    }).await.map_err(|e| {
        let msg = e.to_string();
        if msg.contains("UNIQUE constraint failed") || msg.contains("unique constraint") {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "server name already exists" })),
            );
        }
        tracing::error!(error = %e, "Failed to create MCP server");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "internal error" })),
        )
    })?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(server))))
}

pub async fn get_mcp_server(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    match state.db.get_mcp_server(id).await {
        Ok(Some(server)) => Ok(Json(serde_json::json!(server))),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        )),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get MCP server");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            ))
        }
    }
}

pub async fn update_mcp_server(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateMcpServerRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    match state.db.update_mcp_server(id, req.name, req.url, req.description, req.enabled).await {
        Ok(Some(server)) => Ok(Json(serde_json::json!(server))),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        )),
        Err(e) => {
            tracing::error!(error = %e, "Failed to update MCP server");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            ))
        }
    }
}

pub async fn delete_mcp_server(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    match state.db.delete_mcp_server(id).await {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        )),
        Err(e) => {
            tracing::error!(error = %e, "Failed to delete MCP server");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            ))
        }
    }
}
