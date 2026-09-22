#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::{Experiment, NewExperiment};
use crate::db::repositories::experiments::{
    retention_window_open, ExperimentRepository, ExperimentRow, ExperimentStatusFilter,
};
use super::{PostgresDb, now_utc};

const SELECT_COLS: &str = "id, name, variants, allowed_user_ids, status, feed_learning, \
                           expires_at, created_at, closed_at, retain_content, \
                           content_retention_days";

#[async_trait]
impl ExperimentRepository for PostgresDb {
    async fn create(&self, new: NewExperiment) -> anyhow::Result<Experiment> {
        let now = now_utc();
        let variants = serde_json::to_string(&new.variants)?;
        let allowed = serde_json::to_string(&new.allowed_user_ids)?;
        let row = sqlx::query_as::<_, ExperimentRow>(&format!(
            "INSERT INTO experiments (name, variants, allowed_user_ids, status, feed_learning, \
             expires_at, created_at, closed_at, retain_content, content_retention_days) \
             VALUES ($1, $2, $3, 'active', $4, $5, $6, NULL, $7, $8) \
             RETURNING {SELECT_COLS}"
        ))
        .bind(&new.name)
        .bind(&variants)
        .bind(&allowed)
        .bind(new.feed_learning)
        .bind(new.expires_at)
        .bind(&now)
        .bind(new.retain_content)
        .bind(new.content_retention_days)
        .fetch_one(&self.pool)
        .await?;
        Experiment::try_from(row)
    }

    async fn get(&self, id: i64) -> anyhow::Result<Option<Experiment>> {
        let row = sqlx::query_as::<_, ExperimentRow>(&format!(
            "SELECT {SELECT_COLS} FROM experiments WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Experiment::try_from).transpose()
    }

    async fn list(&self, filter: ExperimentStatusFilter) -> anyhow::Result<Vec<Experiment>> {
        let predicate = match filter {
            ExperimentStatusFilter::All => "TRUE",
            ExperimentStatusFilter::Active => "status = 'active'",
            ExperimentStatusFilter::Closed => "status = 'closed'",
        };
        let rows = sqlx::query_as::<_, ExperimentRow>(&format!(
            "SELECT {SELECT_COLS} FROM experiments WHERE {predicate} ORDER BY id DESC"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Experiment::try_from).collect()
    }

    async fn close(&self, id: i64, closed_at: &str) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE experiments SET status = 'closed', closed_at = $1 \
             WHERE id = $2 AND status = 'active'",
        )
        .bind(closed_at)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn close_expired(&self, now_epoch: i64, closed_at: &str) -> anyhow::Result<Vec<i64>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "UPDATE experiments SET status = 'closed', closed_at = $1 \
             WHERE status = 'active' AND expires_at > 0 AND expires_at <= $2 \
             RETURNING id",
        )
        .bind(closed_at)
        .bind(now_epoch)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn all_retaining_open_or_within_window(&self, now: &str) -> anyhow::Result<Vec<Experiment>> {
        let now = chrono::DateTime::parse_from_rfc3339(now)?.with_timezone(&chrono::Utc);
        let rows = sqlx::query_as::<_, ExperimentRow>(&format!(
            "SELECT {SELECT_COLS} FROM experiments WHERE retain_content ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await?;
        let all: Vec<Experiment> = rows.into_iter().map(Experiment::try_from).collect::<Result<_, _>>()?;
        Ok(all.into_iter().filter(|e| retention_window_open(e, now)).collect())
    }

    async fn closed_retaining(&self) -> anyhow::Result<Vec<Experiment>> {
        let rows = sqlx::query_as::<_, ExperimentRow>(&format!(
            "SELECT {SELECT_COLS} FROM experiments \
             WHERE retain_content AND status = 'closed' ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Experiment::try_from).collect()
    }
}
