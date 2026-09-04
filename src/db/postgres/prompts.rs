#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::{NewPrompt, Prompt};
use crate::db::prompt_store::CONTENT_NOT_STORED;
use crate::db::repositories::costs::ArmFilter;
use crate::db::repositories::prompts::{ExperimentRunLatency, LatencySummary, PromptRepository};
use super::costs::attribution_predicate;
use super::{PostgresDb, now_utc};

/// Rows that carry a real latency measurement. Cache hits are logged with
/// `0` (or `NULL`), and would otherwise pull every percentile toward zero.
const LATENCY_SAMPLE: &str = "latency_ms IS NOT NULL AND latency_ms > 0";

/// Experiment ids bound per `DELETE ... IN (...)` statement in
/// `purge_older_than_except`.
const PURGE_ID_CHUNK: usize = 500;

#[async_trait]
impl PromptRepository for PostgresDb {
    async fn create(&self, prompt: NewPrompt) -> anyhow::Result<Prompt> {
        let now = now_utc();
        let row = sqlx::query_as::<_, Prompt>(
            r#"INSERT INTO prompts (
                user_id, session_id, request_model, routed_model, provider,
                messages, response, finish_reason, prompt_tokens, completion_tokens,
                cache_read_tokens, cache_write_tokens,
                cost_usd, latency_ms, tags, project,
                attribution_correlation_id, attribution_tags,
                experiment_id, experiment_variant, created_at
               ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                         $17, $18, $19, $20, $21)
               RETURNING id, user_id, session_id, request_model, routed_model, provider,
                         messages, response, finish_reason, prompt_tokens, completion_tokens,
                         cache_read_tokens, cache_write_tokens,
                         cost_usd, latency_ms, tags, project,
                         attribution_correlation_id, attribution_tags,
                      experiment_id, experiment_variant, created_at"#,
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
        .bind(prompt.experiment_id)
        .bind(&prompt.experiment_variant)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<Prompt>> {
        let row = sqlx::query_as::<_, Prompt>(
            r#"SELECT id, user_id, session_id, request_model, routed_model, provider,
                      messages, response, finish_reason, prompt_tokens, completion_tokens,
                      cache_read_tokens, cache_write_tokens,
                      cost_usd, latency_ms, tags, project,
                      attribution_correlation_id, attribution_tags,
                      experiment_id, experiment_variant, created_at
               FROM prompts WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list_by_user(&self, user_id: i64, limit: i64) -> anyhow::Result<Vec<Prompt>> {
        let rows = sqlx::query_as::<_, Prompt>(
            r#"SELECT id, user_id, session_id, request_model, routed_model, provider,
                      messages, response, finish_reason, prompt_tokens, completion_tokens,
                      cache_read_tokens, cache_write_tokens,
                      cost_usd, latency_ms, tags, project,
                      attribution_correlation_id, attribution_tags,
                      experiment_id, experiment_variant, created_at
               FROM prompts WHERE user_id = $1 ORDER BY created_at DESC LIMIT $2"#,
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<Prompt>> {
        let rows = sqlx::query_as::<_, Prompt>(
            r#"SELECT id, user_id, session_id, request_model, routed_model, provider,
                      messages, response, finish_reason, prompt_tokens, completion_tokens,
                      cache_read_tokens, cache_write_tokens,
                      cost_usd, latency_ms, tags, project,
                      attribution_correlation_id, attribution_tags,
                      experiment_id, experiment_variant, created_at
               FROM prompts ORDER BY created_at DESC LIMIT $1 OFFSET $2"#,
        )
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
        let result = sqlx::query("DELETE FROM prompts WHERE created_at < $1")
            .bind(cutoff_rfc3339)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    async fn purge_older_than_except(
        &self,
        cutoff_rfc3339: &str,
        except_experiment_ids: &[i64],
    ) -> anyhow::Result<u64> {
        if except_experiment_ids.is_empty() {
            return PromptRepository::purge_older_than(self, cutoff_rfc3339).await;
        }
        // Unstamped rows first, then the stamped ones by experiment. Deleting
        // by a positive `IN` list of the experiments that are NOT protected
        // keeps the statement correct when the list has to be chunked (a
        // `NOT IN` split across chunks would delete a protected row in the
        // chunk that does not name it).
        let result = sqlx::query(
            "DELETE FROM prompts WHERE created_at < $1 AND experiment_id IS NULL",
        )
        .bind(cutoff_rfc3339)
        .execute(&self.pool)
        .await?;
        let mut deleted = result.rows_affected();

        let stamped: Vec<(i64,)> = sqlx::query_as(
            "SELECT DISTINCT experiment_id FROM prompts \
             WHERE created_at < $1 AND experiment_id IS NOT NULL",
        )
        .bind(cutoff_rfc3339)
        .fetch_all(&self.pool)
        .await?;
        let expendable: Vec<i64> = stamped
            .into_iter()
            .map(|(id,)| id)
            .filter(|id| !except_experiment_ids.contains(id))
            .collect();
        for chunk in expendable.chunks(PURGE_ID_CHUNK) {
            let placeholders = (0..chunk.len())
                .map(|i| format!("${}", i + 2))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "DELETE FROM prompts WHERE created_at < $1 AND experiment_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&sql).bind(cutoff_rfc3339);
            for id in chunk {
                q = q.bind(id);
            }
            deleted += q.execute(&self.pool).await?.rows_affected();
        }
        Ok(deleted)
    }

    async fn redact_experiment_content(&self, experiment_id: i64) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "UPDATE prompts SET messages = $1, response = NULL \
             WHERE experiment_id = $2 AND (messages != $1 OR response IS NOT NULL)",
        )
        .bind(CONTENT_NOT_STORED)
        .bind(experiment_id)
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
        let (predicate, binds) = arm_predicate(filter);
        let n = binds.len();
        let where_clause = format!(
            "{} AND created_at >= ${} AND created_at < ${} AND {}",
            predicate,
            n + 1,
            n + 2,
            LATENCY_SAMPLE
        );

        // AVG over BIGINT yields NUMERIC in Postgres; cast so sqlx decodes f64.
        let sql = format!(
            "SELECT COUNT(*), AVG(latency_ms)::float8 FROM prompts WHERE {}",
            where_clause
        );
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
            "SELECT latency_ms FROM prompts WHERE {} ORDER BY latency_ms ASC LIMIT 1 OFFSET ${}",
            where_clause,
            n + 3
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

    async fn experiment_run_latency(
        &self,
        experiment_id: i64,
    ) -> anyhow::Result<Vec<ExperimentRunLatency>> {
        let sql = format!(
            "SELECT user_id, attribution_correlation_id, COUNT(*), AVG(latency_ms)::float8 \
             FROM prompts \
             WHERE experiment_id = $1 AND attribution_correlation_id IS NOT NULL AND {} \
             GROUP BY user_id, attribution_correlation_id \
             ORDER BY user_id ASC, attribution_correlation_id ASC",
            LATENCY_SAMPLE
        );
        let rows = sqlx::query_as::<_, (i64, String, i64, Option<f64>)>(&sql)
            .bind(experiment_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(user_id, correlation_id, samples, mean_ms)| ExperimentRunLatency {
                user_id, correlation_id, samples, mean_ms,
            })
            .collect())
    }

    async fn experiment_content_bytes(&self, experiment_id: i64) -> anyhow::Result<i64> {
        let (bytes,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(octet_length(messages) + octet_length(COALESCE(response, ''))), 0)::BIGINT \
             FROM prompts WHERE experiment_id = $1",
        )
        .bind(experiment_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(bytes)
    }
}

/// Predicate for a comparison arm against `prompts`. Model arms match the
/// model actually served (`routed_model`), not the alias the caller asked for.
fn arm_predicate(filter: &ArmFilter) -> (String, Vec<String>) {
    match filter {
        ArmFilter::Model(m) => ("routed_model = $1".to_string(), vec![m.clone()]),
        ArmFilter::Provider(p) => ("provider = $1".to_string(), vec![p.clone()]),
        ArmFilter::Attribution(f) => attribution_predicate(f),
        ArmFilter::Variant { experiment_id, variant } => (
            "experiment_id = CAST($1 AS BIGINT) AND experiment_variant = $2".to_string(),
            vec![experiment_id.to_string(), variant.clone()],
        ),
    }
}
