use async_trait::async_trait;

use crate::db::models::{NewRequestFailure, RequestFailure};
use crate::db::repositories::costs::ArmFilter;
use crate::db::repositories::failures::{ExperimentRunFailures, FailureRepository};
use super::costs::attribution_predicate;
use super::{SqliteDb, now_utc};

/// Columns selected when reading a failure row back.
const FAILURE_COLUMNS: &str = "id, user_id, api_key_id, endpoint, request_model, routed_model, \
                               provider, stage, status_code, error_message, attempts, latency_ms, \
                               project, attribution_correlation_id, attribution_tags, \
                               experiment_id, experiment_variant, created_at";

#[async_trait]
impl FailureRepository for SqliteDb {
    async fn create(&self, failure: NewRequestFailure) -> anyhow::Result<RequestFailure> {
        let now = now_utc();
        let result = sqlx::query(
            r#"INSERT INTO request_failures (
                user_id, api_key_id, endpoint, request_model, routed_model, provider,
                stage, status_code, error_message, attempts, latency_ms, project,
                attribution_correlation_id, attribution_tags,
                experiment_id, experiment_variant, created_at
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(failure.user_id)
        .bind(failure.api_key_id)
        .bind(&failure.endpoint)
        .bind(&failure.request_model)
        .bind(&failure.routed_model)
        .bind(&failure.provider)
        .bind(failure.stage.as_str())
        .bind(failure.status_code)
        .bind(&failure.error_message)
        .bind(failure.attempts)
        .bind(failure.latency_ms)
        .bind(&failure.project)
        .bind(&failure.attribution_correlation_id)
        .bind(&failure.attribution_tags)
        .bind(failure.experiment_id)
        .bind(&failure.experiment_variant)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, RequestFailure>(&format!(
            "SELECT {} FROM request_failures WHERE id = ?",
            FAILURE_COLUMNS
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<RequestFailure>> {
        let rows = sqlx::query_as::<_, RequestFailure>(&format!(
            "SELECT {} FROM request_failures ORDER BY created_at DESC LIMIT ? OFFSET ?",
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
        let sql = format!(
            "SELECT COUNT(*) FROM request_failures \
             WHERE {} AND created_at >= ? AND created_at < ?",
            predicate
        );
        let mut q = sqlx::query_as::<_, (i64,)>(&sql);
        for b in binds {
            q = q.bind(b);
        }
        let (count,) = q.bind(start).bind(end).fetch_one(&self.pool).await?;
        Ok(count)
    }

    async fn has_rows_for_user(&self, user_id: i64, correlation_id: &str) -> anyhow::Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT 1 FROM request_failures \
             WHERE user_id = ? AND attribution_correlation_id = ? LIMIT 1",
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
             WHERE experiment_id = ? AND user_id IS NOT NULL \
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
            "COALESCE(routed_model, request_model) = ?".to_string(),
            vec![m.clone()],
        ),
        ArmFilter::Provider(p) => ("provider = ?".to_string(), vec![p.clone()]),
        ArmFilter::Attribution(f) => attribution_predicate(f),
        ArmFilter::Variant { experiment_id, variant } => (
            "experiment_id = CAST(? AS INTEGER) AND experiment_variant = ?".to_string(),
            vec![experiment_id.to_string(), variant.clone()],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::costs::{ArmFilter, AttributionFilter};
    use crate::db::sqlite::SqliteDb;

    const W_START: &str = "2026-03-01T00:00:00Z";
    const W_END: &str = "2026-04-01T00:00:00Z";

    async fn make_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    /// Failure row with an explicit `created_at` (the repository's `create`
    /// stamps "now", which would fall outside the test window).
    async fn insert_failure(
        db: &SqliteDb,
        request_model: &str,
        routed_model: Option<&str>,
        provider: Option<&str>,
        run: Option<&str>,
        tags: &str,
        created_at: &str,
    ) {
        sqlx::query(
            "INSERT INTO request_failures (endpoint, request_model, routed_model, provider, \
             stage, error_message, attempts, attribution_correlation_id, attribution_tags, \
             created_at) VALUES ('/v1/chat/completions', ?, ?, ?, 'provider', 'boom', 1, ?, ?, ?)",
        )
        .bind(request_model)
        .bind(routed_model)
        .bind(provider)
        .bind(run)
        .bind(tags)
        .bind(created_at)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn count_for_arm_model_falls_back_to_request_model() {
        let db = make_db().await;
        insert_failure(&db, "alias", Some("X"), Some("p"), None, "{}", "2026-03-02T00:00:00Z").await;
        insert_failure(&db, "X", Some("X"), Some("p"), None, "{}", "2026-03-03T00:00:00Z").await;
        insert_failure(&db, "X", None, None, None, "{}", "2026-03-04T00:00:00Z").await; // failed before routing
        insert_failure(&db, "Y", Some("Y"), Some("p"), None, "{}", "2026-03-05T00:00:00Z").await;
        insert_failure(&db, "X", Some("X"), Some("p"), None, "{}", "2026-04-01T00:00:00Z").await; // outside

        let x = db.count_for_arm(&ArmFilter::Model("X".into()), W_START, W_END).await.unwrap();
        assert_eq!(x, 3);
        let y = db.count_for_arm(&ArmFilter::Model("Y".into()), W_START, W_END).await.unwrap();
        assert_eq!(y, 1);
        let z = db.count_for_arm(&ArmFilter::Model("Z".into()), W_START, W_END).await.unwrap();
        assert_eq!(z, 0);
    }

    #[tokio::test]
    async fn count_for_arm_provider_tag_and_run() {
        let db = make_db().await;
        insert_failure(&db, "X", Some("X"), Some("p1"), Some("run-1"), r#"{"arm":"a"}"#, "2026-03-02T00:00:00Z").await;
        insert_failure(&db, "X", Some("X"), Some("p1"), Some("run-10"), r#"{"arm":"a"}"#, "2026-03-03T00:00:00Z").await;
        insert_failure(&db, "X", Some("X"), Some("p2"), None, r#"{"arm":"b"}"#, "2026-03-04T00:00:00Z").await;
        insert_failure(&db, "X", None, None, None, "{}", "2026-03-05T00:00:00Z").await;

        let p1 = db.count_for_arm(&ArmFilter::Provider("p1".into()), W_START, W_END).await.unwrap();
        assert_eq!(p1, 2);

        let tag = ArmFilter::Attribution(AttributionFilter::Tag { key: "arm".into(), value: "a".into() });
        assert_eq!(db.count_for_arm(&tag, W_START, W_END).await.unwrap(), 2);
        let tag_b = ArmFilter::Attribution(AttributionFilter::Tag { key: "arm".into(), value: "b".into() });
        assert_eq!(db.count_for_arm(&tag_b, W_START, W_END).await.unwrap(), 1);

        let run = ArmFilter::Attribution(AttributionFilter::CorrelationId("run-1".into()));
        assert_eq!(db.count_for_arm(&run, W_START, W_END).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn has_rows_for_user_matches_user_and_correlation_id() {
        let db = make_db().await;
        sqlx::query("INSERT INTO users (id, name, enabled, created_at, metadata) VALUES (1, 'alice', 1, '2026-01-01T00:00:00Z', '{}')")
            .execute(&db.pool).await.unwrap();
        sqlx::query(
            "INSERT INTO request_failures (user_id, endpoint, request_model, stage, error_message, \
             attempts, attribution_correlation_id, attribution_tags, created_at) \
             VALUES (1, '/v1/chat/completions', 'X', 'provider', 'boom', 1, 'run-1', '{}', \
                     '2026-03-02T00:00:00Z')",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        assert!(db.has_rows_for_user(1, "run-1").await.unwrap());
        assert!(!db.has_rows_for_user(2, "run-1").await.unwrap());
        assert!(!db.has_rows_for_user(1, "run-2").await.unwrap());
    }

    #[tokio::test]
    async fn create_writes_and_reads_back_experiment_columns() {
        let db = make_db().await;
        sqlx::query("INSERT INTO users (id, name, enabled, created_at, metadata) VALUES (1, 'alice', 1, '2026-01-01T00:00:00Z', '{}')")
            .execute(&db.pool).await.unwrap();
        let row = db
            .create(crate::db::models::NewRequestFailure {
                user_id: Some(1),
                api_key_id: None,
                endpoint: "/v1/chat/completions".into(),
                request_model: "X".into(),
                routed_model: None,
                provider: None,
                stage: crate::db::models::FailureStage::Resolve,
                status_code: None,
                error_message: "boom".into(),
                attempts: 1,
                latency_ms: None,
                project: None,
                attribution_correlation_id: Some("run-1".into()),
                attribution_tags: "{}".into(),
                experiment_id: Some(4),
                experiment_variant: Some("control".into()),
            })
            .await
            .unwrap();
        assert_eq!(row.experiment_id, Some(4));
        assert_eq!(row.experiment_variant.as_deref(), Some("control"));
        assert_eq!(db.list(10, 0).await.unwrap()[0].experiment_id, Some(4));
    }

    async fn insert_experiment_failure(
        db: &SqliteDb,
        user_id: Option<i64>,
        run: Option<&str>,
        experiment: (i64, &str),
        created_at: &str,
    ) {
        sqlx::query(
            "INSERT INTO request_failures (user_id, endpoint, request_model, stage, error_message, \
             attempts, attribution_correlation_id, attribution_tags, experiment_id, \
             experiment_variant, created_at) \
             VALUES (?, '/v1/chat/completions', 'req', 'provider', 'boom', 1, ?, '{}', ?, ?, ?)",
        )
        .bind(user_id)
        .bind(run)
        .bind(experiment.0)
        .bind(experiment.1)
        .bind(created_at)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn experiment_run_failures_group_by_run_and_variant() {
        let db = make_db().await;
        for (id, name) in [(1, "alice"), (2, "bob")] {
            sqlx::query("INSERT INTO users (id, name, created_at) VALUES (?, ?, '2025-01-01T00:00:00Z')")
                .bind(id)
                .bind(name)
                .execute(&db.pool)
                .await
                .unwrap();
        }
        insert_experiment_failure(&db, Some(1), Some("a"), (7, "control"), "2026-03-02T00:00:00Z").await;
        insert_experiment_failure(&db, Some(1), Some("a"), (7, "control"), "2026-03-04T00:00:00Z").await;
        insert_experiment_failure(&db, Some(1), Some("a"), (7, "candidate"), "2026-03-03T00:00:00Z").await;
        insert_experiment_failure(&db, Some(2), Some("a"), (7, "control"), "2026-03-05T00:00:00Z").await;
        // Without a user or a correlation id there is no run to attach to.
        insert_experiment_failure(&db, None, Some("a"), (7, "control"), "2026-03-05T00:00:00Z").await;
        insert_experiment_failure(&db, Some(1), None, (7, "control"), "2026-03-05T00:00:00Z").await;
        insert_experiment_failure(&db, Some(1), Some("a"), (8, "control"), "2026-03-05T00:00:00Z").await;

        let rows = db.experiment_run_failures(7).await.unwrap();
        let summary: Vec<String> = rows
            .iter()
            .map(|r| {
                format!(
                    "{}/{}/{}:{} {}..{}",
                    r.user_id,
                    r.correlation_id,
                    r.variant.as_deref().unwrap_or("-"),
                    r.failures,
                    r.first_at,
                    r.last_at
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                "1/a/candidate:1 2026-03-03T00:00:00Z..2026-03-03T00:00:00Z",
                "1/a/control:2 2026-03-02T00:00:00Z..2026-03-04T00:00:00Z",
                "2/a/control:1 2026-03-05T00:00:00Z..2026-03-05T00:00:00Z",
            ]
        );

        let variant = ArmFilter::Variant { experiment_id: 7, variant: "control".to_string() };
        assert_eq!(db.count_for_arm(&variant, W_START, W_END).await.unwrap(), 5);
    }
}
