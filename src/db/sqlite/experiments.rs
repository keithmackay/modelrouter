use async_trait::async_trait;

use crate::db::models::{Experiment, NewExperiment};
use crate::db::repositories::experiments::{
    retention_window_open, ExperimentRepository, ExperimentRow, ExperimentStatusFilter,
};
use super::{SqliteDb, now_utc};

const SELECT_COLS: &str = "id, name, variants, allowed_user_ids, status, feed_learning, \
                                      expires_at, created_at, closed_at, retain_content, \
                                      content_retention_days";

#[async_trait]
impl ExperimentRepository for SqliteDb {
    async fn create(&self, new: NewExperiment) -> anyhow::Result<Experiment> {
        let now = now_utc();
        let variants = serde_json::to_string(&new.variants)?;
        let allowed = serde_json::to_string(&new.allowed_user_ids)?;
        let result = sqlx::query(
            "INSERT INTO experiments (name, variants, allowed_user_ids, status, feed_learning, \
             expires_at, created_at, closed_at, retain_content, content_retention_days) \
             VALUES (?, ?, ?, 'active', ?, ?, ?, NULL, ?, ?)",
        )
        .bind(&new.name)
        .bind(&variants)
        .bind(&allowed)
        .bind(new.feed_learning)
        .bind(new.expires_at)
        .bind(&now)
        .bind(new.retain_content)
        .bind(new.content_retention_days)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, ExperimentRow>(&format!(
            "SELECT {SELECT_COLS} FROM experiments WHERE id = ?"
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Experiment::try_from(row)
    }

    async fn get(&self, id: i64) -> anyhow::Result<Option<Experiment>> {
        let row = sqlx::query_as::<_, ExperimentRow>(&format!(
            "SELECT {SELECT_COLS} FROM experiments WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Experiment::try_from).transpose()
    }

    async fn list(&self, filter: ExperimentStatusFilter) -> anyhow::Result<Vec<Experiment>> {
        let predicate = match filter {
            ExperimentStatusFilter::All => "1 = 1",
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
            "UPDATE experiments SET status = 'closed', closed_at = ? \
             WHERE id = ? AND status = 'active'",
        )
        .bind(closed_at)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn close_expired(&self, now_epoch: i64, closed_at: &str) -> anyhow::Result<Vec<i64>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "UPDATE experiments SET status = 'closed', closed_at = ? \
             WHERE status = 'active' AND expires_at > 0 AND expires_at <= ? \
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
            "SELECT {SELECT_COLS} FROM experiments WHERE retain_content = 1 ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await?;
        let all: Vec<Experiment> = rows.into_iter().map(Experiment::try_from).collect::<Result<_, _>>()?;
        Ok(all.into_iter().filter(|e| retention_window_open(e, now)).collect())
    }

    async fn closed_retaining(&self) -> anyhow::Result<Vec<Experiment>> {
        let rows = sqlx::query_as::<_, ExperimentRow>(&format!(
            "SELECT {SELECT_COLS} FROM experiments \
             WHERE retain_content = 1 AND status = 'closed' ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Experiment::try_from).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::db::models::{ExperimentStatus, ExperimentVariants, VariantTarget};

    async fn test_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    fn target(expr: &str, provider: &str, model: &str) -> VariantTarget {
        VariantTarget { target: expr.into(), provider: provider.into(), model: model.into() }
    }

    fn variants() -> ExperimentVariants {
        let mut control = BTreeMap::new();
        control.insert("fast".to_string(), target("fast", "openai", "gpt-5-mini"));
        let mut candidate = BTreeMap::new();
        candidate.insert("fast".to_string(), target("anthropic/claude-haiku", "anthropic", "claude-haiku"));
        candidate.insert("deep".to_string(), target("deep", "anthropic", "claude-opus"));
        let mut v = BTreeMap::new();
        v.insert("control".to_string(), control);
        v.insert("candidate".to_string(), candidate);
        v
    }

    fn new_experiment(name: &str, expires_at: i64) -> NewExperiment {
        NewExperiment {
            name: name.to_string(),
            variants: variants(),
            allowed_user_ids: vec![3, 7],
            feed_learning: true,
            expires_at,
            retain_content: true,
            content_retention_days: 0,
        }
    }

    #[tokio::test]
    async fn create_then_get_round_trips_every_column() {
        let db = test_db().await;
        let created = db.create(new_experiment("exp-a", 0)).await.unwrap();
        let got = db.get(created.id).await.unwrap().expect("row exists");
        assert_eq!(got, created);

        assert_eq!(got.name, "exp-a");
        assert_eq!(got.variants, variants());
        assert_eq!(got.variants["candidate"]["fast"].provider, "anthropic");
        assert_eq!(got.variants["candidate"]["fast"].model, "claude-haiku");
        assert_eq!(got.variants["candidate"]["fast"].target, "anthropic/claude-haiku");
        assert_eq!(got.allowed_user_ids, vec![3, 7]);
        assert_eq!(got.status, ExperimentStatus::Active);
        assert!(got.feed_learning);
        assert_eq!(got.expires_at, 0);
        assert!(!got.created_at.is_empty());
        assert!(got.closed_at.is_none());
        assert!(got.retain_content);
        assert_eq!(got.content_retention_days, 0);

        assert!(db.get(created.id + 100).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_allowed_user_ids_round_trips() {
        let db = test_db().await;
        let mut new = new_experiment("open", 0);
        new.allowed_user_ids = vec![];
        new.feed_learning = false;
        new.retain_content = false;
        new.content_retention_days = 30;
        let created = db.create(new).await.unwrap();
        assert!(created.allowed_user_ids.is_empty());
        assert!(!created.feed_learning);
        assert!(!created.retain_content);
        assert_eq!(created.content_retention_days, 30);
    }

    #[tokio::test]
    async fn duplicate_name_is_an_error() {
        let db = test_db().await;
        db.create(new_experiment("dup", 0)).await.unwrap();
        assert!(db.create(new_experiment("dup", 0)).await.is_err());
        assert_eq!(db.list(ExperimentStatusFilter::All).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_filters_by_status_newest_first() {
        let db = test_db().await;
        let a = db.create(new_experiment("a", 0)).await.unwrap();
        let b = db.create(new_experiment("b", 0)).await.unwrap();
        assert!(db.close(a.id, "2026-09-01T00:00:00+00:00").await.unwrap());

        let all = db.list(ExperimentStatusFilter::All).await.unwrap();
        assert_eq!(all.iter().map(|e| e.id).collect::<Vec<_>>(), vec![b.id, a.id]);
        let active = db.list(ExperimentStatusFilter::Active).await.unwrap();
        assert_eq!(active.iter().map(|e| e.id).collect::<Vec<_>>(), vec![b.id]);
        let closed = db.list(ExperimentStatusFilter::Closed).await.unwrap();
        assert_eq!(closed.iter().map(|e| e.id).collect::<Vec<_>>(), vec![a.id]);
        assert_eq!(closed[0].closed_at.as_deref(), Some("2026-09-01T00:00:00+00:00"));
        assert_eq!(closed[0].status, ExperimentStatus::Closed);
    }

    #[tokio::test]
    async fn close_twice_changes_one_row_once() {
        let db = test_db().await;
        let e = db.create(new_experiment("once", 0)).await.unwrap();
        assert!(db.close(e.id, "2026-09-01T00:00:00+00:00").await.unwrap());
        assert!(!db.close(e.id, "2026-09-02T00:00:00+00:00").await.unwrap());
        let got = db.get(e.id).await.unwrap().unwrap();
        // The first close wins; the second did not touch the row.
        assert_eq!(got.closed_at.as_deref(), Some("2026-09-01T00:00:00+00:00"));
        assert!(!db.close(e.id + 100, "2026-09-01T00:00:00+00:00").await.unwrap());
    }

    #[tokio::test]
    async fn close_expired_closes_only_elapsed_nonzero_expiries() {
        let db = test_db().await;
        let never = db.create(new_experiment("never", 0)).await.unwrap();
        let past = db.create(new_experiment("past", 1_000)).await.unwrap();
        let at_now = db.create(new_experiment("at-now", 2_000)).await.unwrap();
        let future = db.create(new_experiment("future", 3_000)).await.unwrap();
        let already = db.create(new_experiment("already", 500)).await.unwrap();
        assert!(db.close(already.id, "2026-08-01T00:00:00+00:00").await.unwrap());

        let mut closed = db.close_expired(2_000, "2026-09-01T00:00:00+00:00").await.unwrap();
        closed.sort();
        assert_eq!(closed, vec![past.id, at_now.id]);

        assert_eq!(db.get(never.id).await.unwrap().unwrap().status, ExperimentStatus::Active);
        assert_eq!(db.get(future.id).await.unwrap().unwrap().status, ExperimentStatus::Active);
        let past_row = db.get(past.id).await.unwrap().unwrap();
        assert_eq!(past_row.status, ExperimentStatus::Closed);
        assert_eq!(past_row.closed_at.as_deref(), Some("2026-09-01T00:00:00+00:00"));
        // The one closed earlier keeps its original close time.
        let already_row = db.get(already.id).await.unwrap().unwrap();
        assert_eq!(already_row.closed_at.as_deref(), Some("2026-08-01T00:00:00+00:00"));

        // Nothing left to close.
        assert!(db.close_expired(2_000, "2026-09-02T00:00:00+00:00").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn insert_without_expires_at_fails_at_the_sql_level() {
        let db = test_db().await;
        let err = sqlx::query(
            "INSERT INTO experiments (name, variants, status, created_at, content_retention_days) \
             VALUES ('x', '{}', 'active', '2026-09-01T00:00:00+00:00', 0)",
        )
        .execute(&db.pool)
        .await
        .unwrap_err();
        assert!(err.to_string().contains("expires_at"), "{err}");

        let err = sqlx::query(
            "INSERT INTO experiments (name, variants, status, created_at, expires_at) \
             VALUES ('x', '{}', 'active', '2026-09-01T00:00:00+00:00', 0)",
        )
        .execute(&db.pool)
        .await
        .unwrap_err();
        assert!(err.to_string().contains("content_retention_days"), "{err}");
    }

    #[tokio::test]
    async fn retaining_rows_by_window() {
        let db = test_db().await;
        let mut plain = new_experiment("plain", 0);
        plain.retain_content = false;
        db.create(plain).await.unwrap();

        let active = db.create(new_experiment("active", 0)).await.unwrap();
        let forever = db.create(new_experiment("forever", 0)).await.unwrap();
        db.close(forever.id, "2026-01-01T00:00:00+00:00").await.unwrap();

        let mut windowed = new_experiment("windowed", 0);
        windowed.content_retention_days = 10;
        let windowed = db.create(windowed).await.unwrap();
        db.close(windowed.id, "2026-08-25T00:00:00+00:00").await.unwrap();

        let mut elapsed = new_experiment("elapsed", 0);
        elapsed.content_retention_days = 10;
        let elapsed = db.create(elapsed).await.unwrap();
        db.close(elapsed.id, "2026-08-01T00:00:00+00:00").await.unwrap();

        let now = "2026-09-01T00:00:00+00:00";
        let mut open: Vec<i64> = db
            .all_retaining_open_or_within_window(now)
            .await
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
        open.sort();
        assert_eq!(open, vec![active.id, forever.id, windowed.id]);

        let mut closed: Vec<i64> = db.closed_retaining().await.unwrap().iter().map(|e| e.id).collect();
        closed.sort();
        assert_eq!(closed, vec![forever.id, windowed.id, elapsed.id]);
    }
}
