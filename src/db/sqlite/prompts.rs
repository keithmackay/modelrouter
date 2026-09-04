use async_trait::async_trait;

use crate::db::models::{NewPrompt, Prompt};
use crate::db::repositories::costs::ArmFilter;
use crate::db::repositories::prompts::{LatencySummary, PromptRepository};
use super::costs::arm_predicate;
use super::{SqliteDb, now_utc};

/// Rows that carry a real latency measurement. Cache hits are logged with
/// `0` (or `NULL`), and would otherwise pull every percentile toward zero.
const LATENCY_SAMPLE: &str = "latency_ms IS NOT NULL AND latency_ms > 0";

/// Columns selected when reading a prompt row back.
const PROMPT_COLUMNS: &str = "id, user_id, session_id, request_model, routed_model, provider, \
                              messages, response, finish_reason, prompt_tokens, completion_tokens, \
                              cache_read_tokens, cache_write_tokens, cost_usd, latency_ms, tags, \
                              project, attribution_correlation_id, attribution_tags, created_at";

#[async_trait]
impl PromptRepository for SqliteDb {
    async fn create(&self, prompt: NewPrompt) -> anyhow::Result<Prompt> {
        let now = now_utc();
        let result = sqlx::query(
            r#"INSERT INTO prompts (
                user_id, session_id, request_model, routed_model, provider,
                messages, response, finish_reason, prompt_tokens, completion_tokens,
                cache_read_tokens, cache_write_tokens,
                cost_usd, latency_ms, tags, project,
                attribution_correlation_id, attribution_tags, created_at
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(prompt.user_id)
        .bind(prompt.session_id)
        .bind(&prompt.request_model)
        .bind(&prompt.routed_model)
        .bind(&prompt.provider)
        .bind(&prompt.messages)
        .bind(&prompt.response)
        .bind(&prompt.finish_reason)
        .bind(prompt.prompt_tokens)
        .bind(prompt.completion_tokens)
        .bind(prompt.cache_read_tokens)
        .bind(prompt.cache_write_tokens)
        .bind(prompt.cost_usd)
        .bind(prompt.latency_ms)
        .bind(&prompt.tags)
        .bind(&prompt.project)
        .bind(&prompt.attribution_correlation_id)
        .bind(&prompt.attribution_tags)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, Prompt>(&format!(
            "SELECT {} FROM prompts WHERE id = ?",
            PROMPT_COLUMNS
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<Prompt>> {
        let row = sqlx::query_as::<_, Prompt>(&format!(
            "SELECT {} FROM prompts WHERE id = ?",
            PROMPT_COLUMNS
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list_by_user(&self, user_id: i64, limit: i64) -> anyhow::Result<Vec<Prompt>> {
        let rows = sqlx::query_as::<_, Prompt>(&format!(
            "SELECT {} FROM prompts WHERE user_id = ? ORDER BY created_at DESC LIMIT ?",
            PROMPT_COLUMNS
        ))
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<Prompt>> {
        let rows = sqlx::query_as::<_, Prompt>(&format!(
            "SELECT {} FROM prompts ORDER BY created_at DESC LIMIT ? OFFSET ?",
            PROMPT_COLUMNS
        ))
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn count(&self) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM prompts")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    async fn purge_older_than(&self, cutoff_rfc3339: &str) -> anyhow::Result<u64> {
        // created_at is uniformly RFC3339 UTC (now_utc), so string comparison
        // orders correctly — no parsing needed at purge time.
        let result = sqlx::query("DELETE FROM prompts WHERE created_at < ?")
            .bind(cutoff_rfc3339)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    async fn latency_summary(
        &self,
        filter: &ArmFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<LatencySummary> {
        // Model arms match the model actually served, not the alias asked for.
        let (predicate, binds) = arm_predicate(filter, "routed_model");
        let where_clause = format!(
            "{} AND created_at >= ? AND created_at < ? AND {}",
            predicate, LATENCY_SAMPLE
        );

        let sql = format!("SELECT COUNT(*), AVG(latency_ms) FROM prompts WHERE {}", where_clause);
        let mut q = sqlx::query_as::<_, (i64, Option<f64>)>(&sql);
        for b in &binds {
            q = q.bind(b.clone());
        }
        let (samples, mean_ms) = q.bind(start).bind(end).fetch_one(&self.pool).await?;
        if samples == 0 {
            return Ok(LatencySummary::default());
        }

        // Nearest-rank percentiles: one indexed fetch each, offset computed
        // here and bound rather than done in SQL. The count above and these
        // reads are separate statements, so a retention purge in between can
        // leave the offset past the end; fall back to the largest remaining
        // value, and if nothing is left treat the arm as empty.
        let sql = format!(
            "SELECT latency_ms FROM prompts WHERE {} ORDER BY latency_ms ASC LIMIT 1 OFFSET ?",
            where_clause
        );
        let last_sql = format!(
            "SELECT latency_ms FROM prompts WHERE {} ORDER BY latency_ms DESC LIMIT 1",
            where_clause
        );
        let percentile = |q_frac: f64| {
            let offset = LatencySummary::nearest_rank_offset(samples, q_frac);
            let sql = sql.clone();
            let last_sql = last_sql.clone();
            let binds = binds.clone();
            async move {
                let mut q = sqlx::query_as::<_, (i64,)>(&sql);
                for b in &binds {
                    q = q.bind(b.clone());
                }
                let row = q
                    .bind(start)
                    .bind(end)
                    .bind(offset)
                    .fetch_optional(&self.pool)
                    .await?;
                if let Some((v,)) = row {
                    return anyhow::Ok(Some(v));
                }
                let mut q = sqlx::query_as::<_, (i64,)>(&last_sql);
                for b in binds {
                    q = q.bind(b);
                }
                let row = q.bind(start).bind(end).fetch_optional(&self.pool).await?;
                anyhow::Ok(row.map(|(v,)| v))
            }
        };
        let (p50_ms, p95_ms) = tokio::try_join!(percentile(0.5), percentile(0.95))?;
        let (Some(p50_ms), Some(p95_ms)) = (p50_ms, p95_ms) else {
            return Ok(LatencySummary::default());
        };

        Ok(LatencySummary { samples, mean_ms, p50_ms: Some(p50_ms), p95_ms: Some(p95_ms) })
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::costs::{ArmFilter, AttributionFilter};
    use crate::db::repositories::prompts::LatencySummary;
    use crate::db::sqlite::SqliteDb;

    async fn make_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        sqlx::query("INSERT INTO users (id, name, created_at) VALUES (1, 'test', '2025-01-01T00:00:00Z')")
            .execute(&db.pool)
            .await
            .unwrap();
        db
    }

    #[tokio::test]
    async fn cache_tokens_round_trip_and_flag_cached() {
        let db = make_db().await;
        let saved = PromptRepository::create(
            &db,
            NewPrompt {
                user_id: 1,
                session_id: None,
                request_model: "claude-sonnet-4-6".to_string(),
                routed_model: "anthropic/claude-sonnet-4-6".to_string(),
                provider: "anthropic".to_string(),
                messages: "[]".to_string(),
                response: None,
                finish_reason: None,
                prompt_tokens: 100,
                completion_tokens: 50,
                cache_read_tokens: 900,
                cache_write_tokens: 0,
                cost_usd: 0.01,
                latency_ms: None,
                tags: "[]".to_string(),
                project: None,
                attribution_correlation_id: None,
                attribution_tags: "{}".to_string(),
            },
        )
        .await
        .unwrap();

        assert_eq!(saved.cache_read_tokens, 900);
        assert_eq!(saved.cache_write_tokens, 0);
        assert!(saved.is_cached());

        let fetched = PromptRepository::find_by_id(&db, saved.id).await.unwrap().unwrap();
        assert_eq!(fetched.cache_read_tokens, 900);
        assert!(fetched.is_cached());
    }

    #[tokio::test]
    async fn no_cache_tokens_means_not_cached() {
        let db = make_db().await;
        let saved = PromptRepository::create(
            &db,
            NewPrompt {
                user_id: 1,
                session_id: None,
                request_model: "gpt-4o".to_string(),
                routed_model: "openai/gpt-4o".to_string(),
                provider: "openai".to_string(),
                messages: "[]".to_string(),
                response: None,
                finish_reason: None,
                prompt_tokens: 100,
                completion_tokens: 50,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.01,
                latency_ms: None,
                tags: "[]".to_string(),
                project: None,
                attribution_correlation_id: None,
                attribution_tags: "{}".to_string(),
            },
        )
        .await
        .unwrap();

        assert!(!saved.is_cached());
    }

    // ---- latency summary ----------------------------------------------------

    const W_START: &str = "2026-03-01T00:00:00Z";
    const W_END: &str = "2026-04-01T00:00:00Z";

    /// Minimal prompt row with an explicit `created_at` (the repository's
    /// `create` stamps "now", which would fall outside the test window).
    async fn insert_latency_row(
        db: &SqliteDb,
        routed_model: &str,
        provider: &str,
        run: Option<&str>,
        tags: &str,
        latency_ms: Option<i64>,
        created_at: &str,
    ) {
        sqlx::query(
            "INSERT INTO prompts (user_id, request_model, routed_model, provider, messages, \
             prompt_tokens, completion_tokens, cost_usd, latency_ms, tags, \
             attribution_correlation_id, attribution_tags, created_at) \
             VALUES (1, 'req', ?, ?, '[]', 0, 0, 0.0, ?, '[]', ?, ?, ?)",
        )
        .bind(routed_model)
        .bind(provider)
        .bind(latency_ms)
        .bind(run)
        .bind(tags)
        .bind(created_at)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    fn model(m: &str) -> ArmFilter {
        ArmFilter::Model(m.to_string())
    }

    #[tokio::test]
    async fn latency_summary_mean_and_nearest_rank_percentiles() {
        let db = make_db().await;
        for (i, ms) in [100, 200, 300, 400, 1000].iter().enumerate() {
            let ts = format!("2026-03-{:02}T00:00:00Z", i + 2);
            insert_latency_row(&db, "X", "p", None, "{}", Some(*ms), &ts).await;
        }
        // Rows outside the window and for a different model must not count.
        insert_latency_row(&db, "X", "p", None, "{}", Some(5000), "2026-04-01T00:00:00Z").await;
        insert_latency_row(&db, "Y", "p", None, "{}", Some(7000), "2026-03-10T00:00:00Z").await;

        let s = db.latency_summary(&model("X"), W_START, W_END).await.unwrap();
        assert_eq!(s.samples, 5);
        assert_eq!(s.mean_ms, Some(400.0));
        assert_eq!(s.p50_ms, Some(300));
        assert_eq!(s.p95_ms, Some(1000));
    }

    #[tokio::test]
    async fn latency_summary_excludes_cache_hits_and_missing_latency() {
        let db = make_db().await;
        insert_latency_row(&db, "X", "p", None, "{}", Some(0), "2026-03-02T00:00:00Z").await;
        insert_latency_row(&db, "X", "p", None, "{}", None, "2026-03-03T00:00:00Z").await;
        insert_latency_row(&db, "X", "p", None, "{}", Some(250), "2026-03-04T00:00:00Z").await;
        insert_latency_row(&db, "X", "p", None, "{}", Some(750), "2026-03-05T00:00:00Z").await;

        let s = db.latency_summary(&model("X"), W_START, W_END).await.unwrap();
        assert_eq!(s.samples, 2);
        assert_eq!(s.mean_ms, Some(500.0));
        assert_eq!(s.p50_ms, Some(250));
        assert_eq!(s.p95_ms, Some(750));
    }

    #[tokio::test]
    async fn latency_summary_with_no_samples_is_empty() {
        let db = make_db().await;
        insert_latency_row(&db, "X", "p", None, "{}", Some(0), "2026-03-02T00:00:00Z").await;
        insert_latency_row(&db, "Y", "p", None, "{}", Some(300), "2026-03-02T00:00:00Z").await;

        let s = db.latency_summary(&model("X"), W_START, W_END).await.unwrap();
        assert_eq!(s, LatencySummary { samples: 0, mean_ms: None, p50_ms: None, p95_ms: None });
        let s = db.latency_summary(&model("Z"), W_START, W_END).await.unwrap();
        assert_eq!(s, LatencySummary::default());
    }

    #[tokio::test]
    async fn latency_summary_single_sample_is_its_own_percentiles() {
        let db = make_db().await;
        insert_latency_row(&db, "X", "p", None, "{}", Some(420), "2026-03-02T00:00:00Z").await;

        let s = db.latency_summary(&model("X"), W_START, W_END).await.unwrap();
        assert_eq!(s.samples, 1);
        assert_eq!(s.mean_ms, Some(420.0));
        assert_eq!(s.p50_ms, Some(420));
        assert_eq!(s.p95_ms, Some(420));
    }

    #[tokio::test]
    async fn latency_summary_provider_tag_and_run_arms() {
        let db = make_db().await;
        insert_latency_row(&db, "X", "p1", Some("run-1"), r#"{"arm":"a"}"#, Some(100), "2026-03-02T00:00:00Z").await;
        insert_latency_row(&db, "Y", "p1", Some("run-10"), r#"{"arm":"b"}"#, Some(300), "2026-03-03T00:00:00Z").await;
        insert_latency_row(&db, "X", "p2", None, "{}", Some(900), "2026-03-04T00:00:00Z").await;

        let p1 = db.latency_summary(&ArmFilter::Provider("p1".into()), W_START, W_END).await.unwrap();
        assert_eq!(p1.samples, 2);
        assert_eq!(p1.mean_ms, Some(200.0));

        let tag = ArmFilter::Attribution(AttributionFilter::Tag { key: "arm".into(), value: "a".into() });
        let t = db.latency_summary(&tag, W_START, W_END).await.unwrap();
        assert_eq!(t.samples, 1);
        assert_eq!(t.p95_ms, Some(100));

        let run = ArmFilter::Attribution(AttributionFilter::CorrelationId("run-1".into()));
        let r = db.latency_summary(&run, W_START, W_END).await.unwrap();
        assert_eq!(r.samples, 1);
        assert_eq!(r.p50_ms, Some(100));
    }

    #[test]
    fn nearest_rank_offset_is_clamped() {
        assert_eq!(LatencySummary::nearest_rank_offset(5, 0.5), 2);
        assert_eq!(LatencySummary::nearest_rank_offset(5, 0.95), 4);
        assert_eq!(LatencySummary::nearest_rank_offset(1, 0.5), 0);
        assert_eq!(LatencySummary::nearest_rank_offset(1, 0.95), 0);
        assert_eq!(LatencySummary::nearest_rank_offset(4, 0.5), 1);
        assert_eq!(LatencySummary::nearest_rank_offset(0, 0.5), 0);
        assert_eq!(LatencySummary::nearest_rank_offset(3, 1.0), 2);
        assert_eq!(LatencySummary::nearest_rank_offset(3, 0.0), 0);
    }
}
