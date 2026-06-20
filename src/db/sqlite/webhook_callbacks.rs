use async_trait::async_trait;
use crate::db::repositories::webhook_callbacks::{NewWebhookCallback, WebhookCallback, WebhookCallbackRepository};
use super::{SqliteDb, now_utc};

#[derive(sqlx::FromRow)]
struct WebhookRow {
    id: i64,
    name: String,
    url: String,
    events: String,
    secret_header_name: Option<String>,
    secret_header_value: Option<String>,
    enabled: i64,
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
            enabled: r.enabled != 0,
            created_at: r.created_at,
        }
    }
}

#[async_trait]
impl WebhookCallbackRepository for SqliteDb {
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
             FROM webhook_callbacks WHERE enabled = 1 ORDER BY id ASC"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(WebhookCallback::from).collect())
    }

    async fn create_webhook(&self, new: NewWebhookCallback) -> anyhow::Result<WebhookCallback> {
        let now = now_utc();
        let result = sqlx::query(
            "INSERT INTO webhook_callbacks (name, url, events, secret_header_name, secret_header_value, enabled, created_at) \
             VALUES (?, ?, ?, ?, ?, 1, ?)"
        )
        .bind(&new.name)
        .bind(&new.url)
        .bind(&new.events)
        .bind(&new.secret_header_name)
        .bind(&new.secret_header_value)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, WebhookRow>(
            "SELECT id, name, url, events, secret_header_name, secret_header_value, enabled, created_at \
             FROM webhook_callbacks WHERE id = ?"
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(WebhookCallback::from(row))
    }

    async fn delete_webhook(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM webhook_callbacks WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_webhook_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()> {
        sqlx::query("UPDATE webhook_callbacks SET enabled = ? WHERE id = ?")
            .bind(enabled as i64)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::webhook_callbacks::WebhookCallbackRepository;

    async fn test_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    #[tokio::test]
    async fn test_create_and_list() {
        let db = test_db().await;
        let w = db.create_webhook(NewWebhookCallback {
            name: "test".to_string(),
            url: "https://example.com/hook".to_string(),
            events: r#"["completion"]"#.to_string(),
            secret_header_name: None,
            secret_header_value: None,
        }).await.unwrap();
        assert_eq!(w.name, "test");
        assert!(w.enabled);

        let list = db.list_webhooks().await.unwrap();
        assert_eq!(list.len(), 1);
        let enabled = db.list_enabled_webhooks().await.unwrap();
        assert_eq!(enabled.len(), 1);
    }

    #[tokio::test]
    async fn test_set_enabled() {
        let db = test_db().await;
        let w = db.create_webhook(NewWebhookCallback {
            name: "hook".to_string(),
            url: "https://example.com/hook".to_string(),
            events: r#"["completion"]"#.to_string(),
            secret_header_name: None,
            secret_header_value: None,
        }).await.unwrap();
        db.set_webhook_enabled(w.id, false).await.unwrap();
        let enabled = db.list_enabled_webhooks().await.unwrap();
        assert_eq!(enabled.len(), 0);
        let all = db.list_webhooks().await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(!all[0].enabled);
    }

    #[tokio::test]
    async fn test_delete() {
        let db = test_db().await;
        let w = db.create_webhook(NewWebhookCallback {
            name: "hook".to_string(),
            url: "https://example.com/hook".to_string(),
            events: r#"["completion"]"#.to_string(),
            secret_header_name: None,
            secret_header_value: None,
        }).await.unwrap();
        db.delete_webhook(w.id).await.unwrap();
        let list = db.list_webhooks().await.unwrap();
        assert_eq!(list.len(), 0);
    }
}
