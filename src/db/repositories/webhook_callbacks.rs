use async_trait::async_trait;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookCallback {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub events: String,
    pub secret_header_name: Option<String>,
    pub secret_header_value: Option<String>,
    pub enabled: bool,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct NewWebhookCallback {
    pub name: String,
    pub url: String,
    pub events: String,
    pub secret_header_name: Option<String>,
    pub secret_header_value: Option<String>,
}

#[async_trait]
pub trait WebhookCallbackRepository {
    async fn list_webhooks(&self) -> Result<Vec<WebhookCallback>>;
    async fn list_enabled_webhooks(&self) -> Result<Vec<WebhookCallback>>;
    async fn create_webhook(&self, new: NewWebhookCallback) -> Result<WebhookCallback>;
    async fn delete_webhook(&self, id: i64) -> Result<()>;
    async fn set_webhook_enabled(&self, id: i64, enabled: bool) -> Result<()>;
}
