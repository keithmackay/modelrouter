#![cfg(feature = "postgres")]

use async_trait::async_trait;
use crate::db::models::{McpServer, NewMcpServer};
use crate::db::repositories::mcp_servers::McpServerRepository;
use super::{PostgresDb, now_utc};

#[derive(sqlx::FromRow)]
struct McpServerRow {
    id: i64,
    name: String,
    url: String,
    description: Option<String>,
    enabled: bool,
    created_at: String,
}

impl From<McpServerRow> for McpServer {
    fn from(r: McpServerRow) -> Self {
        McpServer {
            id: r.id,
            name: r.name,
            url: r.url,
            description: r.description,
            enabled: r.enabled,
            created_at: r.created_at,
        }
    }
}

#[async_trait]
impl McpServerRepository for PostgresDb {
    async fn create_mcp_server(&self, server: NewMcpServer) -> anyhow::Result<McpServer> {
        let now = now_utc();
        let row = sqlx::query_as::<_, McpServerRow>(
            r#"INSERT INTO mcp_servers (name, url, description, enabled, created_at)
               VALUES ($1, $2, $3, true, $4)
               RETURNING id, name, url, description, enabled, created_at"#
        )
        .bind(&server.name)
        .bind(&server.url)
        .bind(&server.description)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(McpServer::from(row))
    }

    async fn list_mcp_servers(&self) -> anyhow::Result<Vec<McpServer>> {
        let rows = sqlx::query_as::<_, McpServerRow>(
            "SELECT id, name, url, description, enabled, created_at FROM mcp_servers ORDER BY id"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(McpServer::from).collect())
    }

    async fn get_mcp_server(&self, id: i64) -> anyhow::Result<Option<McpServer>> {
        let row = sqlx::query_as::<_, McpServerRow>(
            "SELECT id, name, url, description, enabled, created_at FROM mcp_servers WHERE id = $1"
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(McpServer::from))
    }

    async fn update_mcp_server(
        &self,
        id: i64,
        name: Option<String>,
        url: Option<String>,
        description: Option<String>,
        enabled: Option<bool>,
    ) -> anyhow::Result<Option<McpServer>> {
        let mut sets: Vec<String> = Vec::new();
        let mut param_idx: i32 = 1;

        if name.is_some()        { sets.push(format!("name = ${}", param_idx)); param_idx += 1; }
        if url.is_some()         { sets.push(format!("url = ${}", param_idx)); param_idx += 1; }
        if description.is_some() { sets.push(format!("description = ${}", param_idx)); param_idx += 1; }
        if enabled.is_some()     { sets.push(format!("enabled = ${}", param_idx)); param_idx += 1; }

        if sets.is_empty() {
            return self.get_mcp_server(id).await;
        }

        let sql = format!(
            "UPDATE mcp_servers SET {} WHERE id = ${} RETURNING id, name, url, description, enabled, created_at",
            sets.join(", "),
            param_idx
        );

        let mut q = sqlx::query_as::<_, McpServerRow>(&sql);
        if let Some(v) = &name        { q = q.bind(v); }
        if let Some(v) = &url         { q = q.bind(v); }
        if let Some(v) = &description { q = q.bind(v); }
        if let Some(v) = enabled      { q = q.bind(v); }
        q = q.bind(id);

        let row = q.fetch_optional(&self.pool).await?;
        Ok(row.map(McpServer::from))
    }

    async fn delete_mcp_server(&self, id: i64) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_servers WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}
