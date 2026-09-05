#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::{NewRequestFailure, RequestFailure};
use crate::db::repositories::costs::ArmFilter;
use crate::db::repositories::failures::{ExperimentRunFailures, FailureRepository};
use super::costs::{attribution_predicate, variant_predicate};
use super::{PostgresDb, now_utc};

const FAILURE_COLUMNS: &str = "id, user_id, api_key_id, endpoint, request_model, routed_model, \
                               provider, stage, status_code, error_message, attempts, latency_ms, \
                               project, attribution_correlation_id, attribution_tags, \
                               experiment_id, experiment_variant, created_at";

#[async_trait]
impl FailureRepository for PostgresDb {
    async fn create(&self, failure: NewRequestFailure) -> anyhow::Result<RequestFailure> {
        let now = now_utc();
        let row = sqlx::query_as::<_, RequestFailure>(
            r#"INSERT INTO request_failures (
                user_id, api_key_id, endpoint, request_model, routed_model, provider,
                stage, status_code, error_message, attempts, latency_ms, project,
                attribution_correlation_id, attribution_tags,
                experiment_id, experiment_variant, created_at
               ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
               RETURNING id, user_id, api_key_id, endpoint, request_model, routed_model,
                         provider, stage, status_code, error_message, attempts, latency_ms,
                         project, attribution_correlation_id, attribution_tags,
                         experiment_id, experiment_variant, created_at"#,
        )
        .bind(failure.user_id)
        .bind(failure.api_key_id)
        .bind(&failure.endpoint)
        .bind(&failure.request_model)
        .bind(&failure.routed_model)
        .bind(&failure.provider)
        .bind(failure.stage.as_str())
        .bind(failure.status_code.map(|c| c as i32))
        .bind(&failure.error_message)
        .bind(failure.attempts as i32)
        .bind(failure.latency_ms)
        .bind(&failure.project)
        .bind(&failure.attribution_correlation_id)
        .bind(&failure.attribution_tags)
        .bind(failure.experiment_id)
        .bind(&failure.experiment_variant)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<RequestFailure>> {
        let rows = sqlx::query_as::<_, RequestFailure>(&format!(
            "SELECT {} FROM request_failures ORDER BY created_at DESC LIMIT $1 OFFSET $2",
            FAILURE_COLUMNS
        ))
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn count(&self) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM request_failures")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    async fn count_by_stage(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT stage, COUNT(*) FROM request_failures GROUP BY stage ORDER BY COUNT(*) DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn count_for_arm(&self, filter: &ArmFilter, start: &str, end: &str) -> anyhow::Result<i64> {
        let (predicate, binds) = arm_predicate(filter);
        let n = binds.len();
        let sql = format!(
            "SELECT COUNT(*) FROM request_failures \
             WHERE {} AND created_at >= ${} AND created_at < ${}",
            predicate,
            n + 1,
            n + 2
        );
        let mut q = sqlx::query_as::<_, (i64,)>(&sql);
        for b in binds {
            q = q.bind(b);
        }
        let (count,) = q.bind(start).bind(end).fetch_one(&self.pool).await?;
        Ok(count)
    }

    async fn has_rows_for_user(&self, user_id: i64, correlation_id: &str) -> anyhow::Result<bool> {
        let row: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM request_failures \
             WHERE user_id = $1 AND attribution_correlation_id = $2 LIMIT 1",
        )
        .bind(user_id)
        .bind(correlation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn experiment_run_failures(
        &self,
        experiment_id: i64,
    ) -> anyhow::Result<Vec<ExperimentRunFailures>> {
        let rows = sqlx::query_as::<_, (i64, String, Option<String>, i64, String, String)>(
            "SELECT user_id, attribution_correlation_id, experiment_variant, COUNT(*), \
                    MIN(created_at), MAX(created_at) \
             FROM request_failures \
             WHERE experiment_id = $1 AND user_id IS NOT NULL \
               AND attribution_correlation_id IS NOT NULL \
             GROUP BY user_id, attribution_correlation_id, experiment_variant \
             ORDER BY user_id ASC, attribution_correlation_id ASC, experiment_variant ASC",
        )
        .bind(experiment_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(user_id, correlation_id, variant, failures, first_at, last_at)| {
                ExperimentRunFailures { user_id, correlation_id, variant, failures, first_at, last_at }
            })
            .collect())
    }
}

/// Predicate for a comparison arm against `request_failures`. A request that
/// failed before routing has no `routed_model`, so model arms fall back to
/// the model the caller asked for.
fn arm_predicate(filter: &ArmFilter) -> (String, Vec<String>) {
    match filter {
        ArmFilter::Model(m) => (
            "COALESCE(routed_model, request_model) = $1".to_string(),
            vec![m.clone()],
        ),
        ArmFilter::Provider(p) => ("provider = $1".to_string(), vec![p.clone()]),
        ArmFilter::Attribution(f) => attribution_predicate(f),
        ArmFilter::Variant { experiment_id, variant } => variant_predicate(*experiment_id, variant),
    }
}
