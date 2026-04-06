use async_trait::async_trait;
use crate::db::models::{McpServer, NewMcpServer};
use crate::db::repositories::mcp_servers::McpServerRepository;
use super::{SqliteDb, now_utc};

#[derive(sqlx::FromRow)]
struct McpServerRow {
    id: i64,
    name: String,
    url: String,
    description: Option<String>,
    enabled: i64,
    created_at: String,
}

impl From<McpServerRow> for McpServer {
    fn from(r: McpServerRow) -> Self {
        McpServer {
            id: r.id,
            name: r.name,
            url: r.url,
            description: r.description,
            enabled: r.enabled != 0,
            created_at: r.created_at,
        }
    }
}

#[async_trait]
impl McpServerRepository for SqliteDb {
    async fn create_mcp_server(&self, server: NewMcpServer) -> anyhow::Result<McpServer> {
        let now = now_utc();
        let result = sqlx::query(
            "INSERT INTO mcp_servers (name, url, description, enabled, created_at) VALUES (?, ?, ?, 1, ?)"
        )
        .bind(&server.name)
        .bind(&server.url)
        .bind(&server.description)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, McpServerRow>(
            "SELECT id, name, url, description, enabled, created_at FROM mcp_servers WHERE id = ?"
        )
        .bind(id)
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
            "SELECT id, name, url, description, enabled, created_at FROM mcp_servers WHERE id = ?"
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
        let mut sets: Vec<&str> = Vec::new();
        if name.is_some()        { sets.push("name = ?"); }
        if url.is_some()         { sets.push("url = ?"); }
        if description.is_some() { sets.push("description = ?"); }
        if enabled.is_some()     { sets.push("enabled = ?"); }

        if sets.is_empty() {
            return self.get_mcp_server(id).await;
        }

        let sql = format!(
            "UPDATE mcp_servers SET {} WHERE id = ?",
            sets.join(", ")
        );

        let mut q = sqlx::query(&sql);
        if let Some(v) = &name        { q = q.bind(v); }
        if let Some(v) = &url         { q = q.bind(v); }
        if let Some(v) = &description { q = q.bind(v); }
        if let Some(v) = enabled      { q = q.bind(v as i64); }
        q = q.bind(id);

        let rows_affected = q.execute(&self.pool).await?.rows_affected();
        if rows_affected == 0 {
            return Ok(None);
        }
        self.get_mcp_server(id).await
    }

    async fn delete_mcp_server(&self, id: i64) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_servers WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;
    use crate::db::repositories::mcp_servers::McpServerRepository;
    use crate::db::models::NewMcpServer;

    async fn test_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    #[tokio::test]
    async fn test_create_and_list() {
        let db = test_db().await;
        let server = db.create_mcp_server(NewMcpServer {
            name: "test".to_string(),
            url: "https://example.com".to_string(),
            description: None,
        }).await.unwrap();
        assert_eq!(server.name, "test");
        assert!(server.enabled);

        let list = db.list_mcp_servers().await.unwrap();
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn test_get() {
        let db = test_db().await;
        let created = db.create_mcp_server(NewMcpServer {
            name: "get-test".to_string(),
            url: "https://example.com".to_string(),
            description: Some("desc".to_string()),
        }).await.unwrap();

        let found = db.get_mcp_server(created.id).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().description, Some("desc".to_string()));

        let missing = db.get_mcp_server(999).await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_update() {
        let db = test_db().await;
        let created = db.create_mcp_server(NewMcpServer {
            name: "update-test".to_string(),
            url: "https://old.com".to_string(),
            description: None,
        }).await.unwrap();

        let updated = db.update_mcp_server(
            created.id,
            None,
            Some("https://new.com".to_string()),
            None,
            Some(false),
        ).await.unwrap();
        let updated = updated.unwrap();
        assert_eq!(updated.url, "https://new.com");
        assert!(!updated.enabled);
    }

    #[tokio::test]
    async fn test_delete() {
        let db = test_db().await;
        let created = db.create_mcp_server(NewMcpServer {
            name: "delete-test".to_string(),
            url: "https://example.com".to_string(),
            description: None,
        }).await.unwrap();

        let deleted = db.delete_mcp_server(created.id).await.unwrap();
        assert!(deleted);

        let not_deleted = db.delete_mcp_server(999).await.unwrap();
        assert!(!not_deleted);

        let list = db.list_mcp_servers().await.unwrap();
        assert!(list.is_empty());
    }
}
