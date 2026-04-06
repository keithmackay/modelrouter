use async_trait::async_trait;
use crate::db::models::{McpServer, NewMcpServer};

#[async_trait]
pub trait McpServerRepository: Send + Sync {
    async fn create_mcp_server(&self, server: NewMcpServer) -> anyhow::Result<McpServer>;
    async fn list_mcp_servers(&self) -> anyhow::Result<Vec<McpServer>>;
    async fn get_mcp_server(&self, id: i64) -> anyhow::Result<Option<McpServer>>;
    async fn update_mcp_server(
        &self,
        id: i64,
        name: Option<String>,
        url: Option<String>,
        description: Option<String>,
        enabled: Option<bool>,
    ) -> anyhow::Result<Option<McpServer>>;
    async fn delete_mcp_server(&self, id: i64) -> anyhow::Result<bool>;
}
