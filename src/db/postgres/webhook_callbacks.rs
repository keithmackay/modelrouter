#![cfg(feature = "postgres")]

use async_trait::async_trait;
use crate::db::repositories::webhook_callbacks::{NewWebhookCallback, WebhookCallback, WebhookCallbackRepository};
use super::{PostgresDb, now_utc};

#[derive(sqlx::FromRow)]
struct WebhookRow {
    id: i64,
    name: String,
    url: String,
    events: String,
    secret_header_name: Option<String>,
    secret_header_value: Option<String>,
    enabled: bool,
    created_at: String,
}

impl From<WebhookRow> for WebhookCallback {
    fn from(r: WebhookRow) -> Self {
        WebhookCallback {
            id: r.id,
            name: r.name,
            url: r.url,
            events: r.events,
            secret_header_name: r.secret_header_name,
            secret_header_value: r.secret_header_value,
            enabled: r.enabled,
            created_at: r.created_at,
        }
    }
}

#[async_trait]
impl WebhookCallbackRepository for PostgresDb {
    async fn list_webhooks(&self) -> anyhow::Result<Vec<WebhookCallback>> {
        let rows = sqlx::query_as::<_, WebhookRow>(
            "SELECT id, name, url, events, secret_header_name, secret_header_value, enabled, created_at \
             FROM webhook_callbacks ORDER BY id ASC"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(WebhookCallback::from).collect())
    }

    async fn list_enabled_webhooks(&self) -> anyhow::Result<Vec<WebhookCallback>> {
        let rows = sqlx::query_as::<_, WebhookRow>(
            "SELECT id, name, url, events, secret_header_name, secret_header_value, enabled, created_at \
             FROM webhook_callbacks WHERE enabled = true ORDER BY id ASC"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(WebhookCallback::from).collect())
    }

    async fn create_webhook(&self, new: NewWebhookCallback) -> anyhow::Result<WebhookCallback> {
        let now = now_utc();
        let row = sqlx::query_as::<_, WebhookRow>(
            r#"INSERT INTO webhook_callbacks (name, url, events, secret_header_name, secret_header_value, enabled, created_at)
               VALUES ($1, $2, $3, $4, $5, true, $6)
               RETURNING id, name, url, events, secret_header_name, secret_header_value, enabled, created_at"#
        )
        .bind(&new.name)
        .bind(&new.url)
        .bind(&new.events)
        .bind(&new.secret_header_name)
        .bind(&new.secret_header_value)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(WebhookCallback::from(row))
    }

    async fn delete_webhook(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM webhook_callbacks WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_webhook_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()> {
        sqlx::query("UPDATE webhook_callbacks SET enabled = $1 WHERE id = $2")
            .bind(enabled)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
