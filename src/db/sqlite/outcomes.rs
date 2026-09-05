use async_trait::async_trait;

use crate::db::models::{NewRunOutcome, RunOutcome};
use crate::db::repositories::outcomes::OutcomeRepository;
use super::{SqliteDb, now_utc};

const SELECT_COLS: &str = "user_id, attribution_correlation_id, outcome, score, rating, \
                                      note, experiment_id, experiment_variant, created_at, updated_at";

#[async_trait]
impl OutcomeRepository for SqliteDb {
    async fn upsert(&self, outcome: NewRunOutcome) -> anyhow::Result<RunOutcome> {
        let now = now_utc();
        sqlx::query(
            "INSERT INTO run_outcomes (user_id, attribution_correlation_id, outcome, score, rating, \
             note, experiment_id, experiment_variant, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, attribution_correlation_id) DO UPDATE SET \
                 outcome = excluded.outcome, score = excluded.score, rating = excluded.rating, \
                 note = excluded.note, experiment_id = excluded.experiment_id, \
                 experiment_variant = excluded.experiment_variant, updated_at = excluded.updated_at",
        )
        .bind(outcome.user_id)
        .bind(&outcome.attribution_correlation_id)
        .bind(&outcome.outcome)
        .bind(outcome.score)
        .bind(outcome.rating)
        .bind(&outcome.note)
        .bind(outcome.experiment_id)
        .bind(&outcome.experiment_variant)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let row = sqlx::query_as::<_, RunOutcome>(&format!(
            "SELECT {SELECT_COLS} FROM run_outcomes \
             WHERE user_id = ? AND attribution_correlation_id = ?"
        ))
        .bind(outcome.user_id)
        .bind(&outcome.attribution_correlation_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get(&self, user_id: i64, correlation_id: &str) -> anyhow::Result<Option<RunOutcome>> {
        let row = sqlx::query_as::<_, RunOutcome>(&format!(
            "SELECT {SELECT_COLS} FROM run_outcomes \
             WHERE user_id = ? AND attribution_correlation_id = ?"
        ))
        .bind(user_id)
        .bind(correlation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn for_experiment(&self, experiment_id: i64) -> anyhow::Result<Vec<RunOutcome>> {
        let rows = sqlx::query_as::<_, RunOutcome>(&format!(
            "SELECT {SELECT_COLS} FROM run_outcomes WHERE experiment_id = ? \
             ORDER BY user_id, attribution_correlation_id"
        ))
        .bind(experiment_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn clear_notes(&self, experiment_id: i64) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "UPDATE run_outcomes SET note = NULL WHERE experiment_id = ? AND note IS NOT NULL",
        )
        .bind(experiment_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        for (id, name) in [(1, "alice"), (2, "bob")] {
            sqlx::query("INSERT INTO users (id, name, enabled, created_at, metadata) VALUES (?, ?, 1, '2026-01-01T00:00:00Z', '{}')")
                .bind(id)
                .bind(name)
                .execute(&db.pool)
                .await
                .unwrap();
        }
        db
    }

    fn outcome(user_id: i64, run: &str, outcome: &str, experiment_id: Option<i64>) -> NewRunOutcome {
        NewRunOutcome {
            user_id,
            attribution_correlation_id: run.to_string(),
            outcome: outcome.to_string(),
            score: Some(0.5),
            rating: Some(3),
            note: Some("first pass".to_string()),
            experiment_id,
            experiment_variant: experiment_id.map(|_| "control".to_string()),
        }
    }

    #[tokio::test]
    async fn upsert_replaces_the_earlier_report() {
        let db = test_db().await;
        let first = db.upsert(outcome(1, "run-1", "failure", Some(9))).await.unwrap();
        assert_eq!(first.outcome, "failure");
        assert_eq!(first.score, Some(0.5));
        assert_eq!(first.rating, Some(3));
        assert_eq!(first.note.as_deref(), Some("first pass"));
        assert_eq!(first.experiment_id, Some(9));
        assert_eq!(first.experiment_variant.as_deref(), Some("control"));
        assert_eq!(first.created_at, first.updated_at);

        let mut second = outcome(1, "run-1", "success", Some(9));
        second.score = Some(0.9);
        second.rating = None;
        second.note = None;
        second.experiment_variant = Some("candidate".to_string());
        let replaced = db.upsert(second).await.unwrap();
        assert_eq!(replaced.outcome, "success");
        assert_eq!(replaced.score, Some(0.9));
        assert_eq!(replaced.rating, None);
        assert_eq!(replaced.note, None);
        assert_eq!(replaced.experiment_variant.as_deref(), Some("candidate"));
        assert_eq!(replaced.created_at, first.created_at);

        let got = db.get(1, "run-1").await.unwrap().unwrap();
        assert_eq!(got, replaced);
        assert_eq!(db.for_experiment(9).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn outcomes_are_keyed_by_user_and_correlation_id() {
        let db = test_db().await;
        db.upsert(outcome(1, "run-1", "success", Some(9))).await.unwrap();
        db.upsert(outcome(2, "run-1", "failure", Some(9))).await.unwrap();
        db.upsert(outcome(1, "run-2", "failure", None)).await.unwrap();

        assert_eq!(db.get(1, "run-1").await.unwrap().unwrap().outcome, "success");
        assert_eq!(db.get(2, "run-1").await.unwrap().unwrap().outcome, "failure");
        assert!(db.get(3, "run-1").await.unwrap().is_none());
        assert!(db.get(1, "run-9").await.unwrap().is_none());

        let for_exp = db.for_experiment(9).await.unwrap();
        assert_eq!(for_exp.len(), 2);
        assert!(db.for_experiment(8).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn clear_notes_only_touches_the_experiment() {
        let db = test_db().await;
        db.upsert(outcome(1, "run-1", "success", Some(9))).await.unwrap();
        db.upsert(outcome(1, "run-2", "success", Some(9))).await.unwrap();
        db.upsert(outcome(1, "run-3", "success", Some(8))).await.unwrap();
        db.upsert(outcome(1, "run-4", "success", None)).await.unwrap();

        assert_eq!(db.clear_notes(9).await.unwrap(), 2);
        assert!(db.get(1, "run-1").await.unwrap().unwrap().note.is_none());
        assert!(db.get(1, "run-2").await.unwrap().unwrap().note.is_none());
        assert_eq!(db.get(1, "run-3").await.unwrap().unwrap().note.as_deref(), Some("first pass"));
        assert_eq!(db.get(1, "run-4").await.unwrap().unwrap().note.as_deref(), Some("first pass"));
        // The rest of the row survives.
        assert_eq!(db.get(1, "run-1").await.unwrap().unwrap().outcome, "success");
        assert_eq!(db.clear_notes(9).await.unwrap(), 0);
    }
}
