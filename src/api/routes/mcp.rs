use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use crate::providers::embedding::EmbeddingRequest;

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

#[derive(Deserialize)]
pub struct DiscoverRequest {
    pub prompt: String,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

fn default_top_k() -> usize { 5 }

#[derive(Serialize)]
pub struct DiscoverResult {
    pub server: crate::db::models::McpServer,
    pub score: f32,
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
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

pub async fn discover_mcp_tools(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
    Json(req): Json<DiscoverRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let servers = state.db.list_mcp_servers().await.map_err(|e| {
        tracing::error!(error = %e, "Failed to list MCP servers for discover");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "internal error" })))
    })?;

    let enabled: Vec<_> = servers.into_iter().filter(|s| s.enabled).collect();

    if enabled.is_empty() {
        return Ok(Json(serde_json::json!({ "results": [] })));
    }

    // Build texts: prompt first, then one text per server
    let mut texts = vec![req.prompt.clone()];
    for s in &enabled {
        texts.push(s.description.clone().unwrap_or_else(|| s.name.clone()));
    }

    let embed_adapter = state.embedding_registry.get("openai").map_err(|e| {
        tracing::warn!(error = %e, "No embedding adapter available for discover");
        (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": "embedding provider unavailable" })))
    })?;

    let embed_req = EmbeddingRequest {
        model: "text-embedding-3-small".to_string(),
        input: texts,
    };
    let result = embed_adapter.embed(&embed_req).await.map_err(|e| {
        tracing::error!(error = %e, "Embedding call failed during discover");
        (StatusCode::BAD_GATEWAY, Json(serde_json::json!({ "error": "embedding failed" })))
    })?;

    let prompt_vec = &result.embeddings[0];
    let top_k = req.top_k.min(enabled.len());

    let mut scored: Vec<DiscoverResult> = enabled
        .into_iter()
        .enumerate()
        .map(|(i, server)| {
            let score = cosine_similarity(prompt_vec, &result.embeddings[i + 1]);
            DiscoverResult { server, score }
        })
        .collect();

    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);

    Ok(Json(serde_json::json!({ "results": scored })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0_f32, 0.0, 0.0];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);

        let b = vec![0.0_f32, 1.0, 0.0];
        assert!((cosine_similarity(&a, &b)).abs() < 1e-6);
    }
}
