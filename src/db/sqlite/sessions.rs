use async_trait::async_trait;

use crate::db::models::{NewSession, Session};
use crate::db::repositories::sessions::SessionRepository;
use super::{SqliteDb, now_utc};

#[async_trait]
impl SessionRepository for SqliteDb {
    async fn find_or_create(&self, session: NewSession) -> anyhow::Result<Session> {
        // Try to find existing session by user_id + external_id
        if let Some(ref ext_id) = session.external_id {
            let existing = sqlx::query_as::<_, Session>(
                "SELECT id, user_id, external_id, project, created_at, last_seen, metadata
                 FROM sessions WHERE user_id = ? AND external_id = ? LIMIT 1",
            )
            .bind(session.user_id)
            .bind(ext_id)
            .fetch_optional(&self.pool)
            .await?;

            if let Some(s) = existing {
                return Ok(s);
            }
        }

        let now = now_utc();
        let result = sqlx::query(
            "INSERT INTO sessions (user_id, external_id, project, created_at, last_seen, metadata)
             VALUES (?, ?, ?, ?, ?, '{}')",
        )
        .bind(session.user_id)
        .bind(&session.external_id)
        .bind(&session.project)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, Session>(
            "SELECT id, user_id, external_id, project, created_at, last_seen, metadata
             FROM sessions WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn update_last_seen(&self, id: i64) -> anyhow::Result<()> {
        let now = now_utc();
        sqlx::query("UPDATE sessions SET last_seen = ? WHERE id = ?")
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
