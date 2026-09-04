use async_trait::async_trait;

use crate::db::models::{NewRequestFailure, RequestFailure};
use crate::db::repositories::costs::ArmFilter;
use crate::db::repositories::failures::FailureRepository;
use super::costs::arm_predicate;
use super::{SqliteDb, now_utc};

/// Columns selected when reading a failure row back.
const FAILURE_COLUMNS: &str = "id, user_id, api_key_id, endpoint, request_model, routed_model, \
                               provider, stage, status_code, error_message, attempts, latency_ms, \
                               project, attribution_correlation_id, attribution_tags, created_at";

#[async_trait]
impl FailureRepository for SqliteDb {
    async fn create(&self, failure: NewRequestFailure) -> anyhow::Result<RequestFailure> {
        let now = now_utc();
        let result = sqlx::query(
            r#"INSERT INTO request_failures (
                user_id, api_key_id, endpoint, request_model, routed_model, provider,
                stage, status_code, error_message, attempts, latency_ms, project,
                attribution_correlation_id, attribution_tags, created_at
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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
        // A request that failed before routing has no routed_model, so model
        // arms fall back to the model the caller asked for.
        let (predicate, binds) = arm_predicate(filter, "COALESCE(routed_model, request_model)");
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
}
