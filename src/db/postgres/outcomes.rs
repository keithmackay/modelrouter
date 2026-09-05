#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::{NewRunOutcome, RunOutcome};
use crate::db::repositories::outcomes::OutcomeRepository;
use super::{PostgresDb, now_utc};

const SELECT_COLS: &str = "user_id, attribution_correlation_id, outcome, score, rating, \
                           note, experiment_id, experiment_variant, created_at, updated_at";

#[async_trait]
impl OutcomeRepository for PostgresDb {
    async fn upsert(&self, outcome: NewRunOutcome) -> anyhow::Result<RunOutcome> {
        let now = now_utc();
        let row = sqlx::query_as::<_, RunOutcome>(&format!(
            "INSERT INTO run_outcomes (user_id, attribution_correlation_id, outcome, score, rating, \
             note, experiment_id, experiment_variant, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9) \
             ON CONFLICT (user_id, attribution_correlation_id) DO UPDATE SET \
                 outcome = EXCLUDED.outcome, score = EXCLUDED.score, rating = EXCLUDED.rating, \
                 note = EXCLUDED.note, experiment_id = EXCLUDED.experiment_id, \
                 experiment_variant = EXCLUDED.experiment_variant, updated_at = EXCLUDED.updated_at \
             RETURNING {SELECT_COLS}"
        ))
        .bind(outcome.user_id)
        .bind(&outcome.attribution_correlation_id)
        .bind(&outcome.outcome)
        .bind(outcome.score)
        .bind(outcome.rating)
        .bind(&outcome.note)
        .bind(outcome.experiment_id)
        .bind(&outcome.experiment_variant)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get(&self, user_id: i64, correlation_id: &str) -> anyhow::Result<Option<RunOutcome>> {
        let row = sqlx::query_as::<_, RunOutcome>(&format!(
            "SELECT {SELECT_COLS} FROM run_outcomes \
             WHERE user_id = $1 AND attribution_correlation_id = $2"
        ))
        .bind(user_id)
        .bind(correlation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn for_experiment(&self, experiment_id: i64) -> anyhow::Result<Vec<RunOutcome>> {
        let rows = sqlx::query_as::<_, RunOutcome>(&format!(
            "SELECT {SELECT_COLS} FROM run_outcomes WHERE experiment_id = $1 \
             ORDER BY user_id, attribution_correlation_id"
        ))
        .bind(experiment_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn clear_notes(&self, experiment_id: i64) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "UPDATE run_outcomes SET note = NULL WHERE experiment_id = $1 AND note IS NOT NULL",
        )
        .bind(experiment_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
