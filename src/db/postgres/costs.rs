#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::{CostLedgerEntry, NewCostLedgerEntry, RunStamp};
use crate::db::repositories::costs::{
    ArmFilter, AttributionBreakdownRow, AttributionFilter, AttributionTotals,
    CacheUsageSummary, CostRepository, ExperimentRunRow, ExperimentRunKey,
    ExperimentUnboundRow, ExperimentVariantModelRow, ExperimentVariantTotals,
};
use super::{PostgresDb, now_utc};

/// Postgres stores `cache_hit` as BOOLEAN, so it must be cast before SUM.
const CACHE_HIT_SUM: &str = "COALESCE(SUM(CASE WHEN cache_hit THEN 1 ELSE 0 END), 0)";

#[async_trait]
impl CostRepository for PostgresDb {
    async fn create(&self, entry: NewCostLedgerEntry) -> anyhow::Result<CostLedgerEntry> {
        let now = now_utc();
        let row = sqlx::query_as::<_, CostLedgerEntry>(
            r#"INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project,
                                        tokens_in, tokens_out, cost_usd, api_key_id, created_at,
                                        attribution_correlation_id, attribution_tags,
                                        experiment_id, experiment_variant, tokens_estimated)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
               RETURNING id, user_id, prompt_id, model, provider, project,
                         tokens_in, tokens_out, cost_usd, created_at, api_key_id,
                         cache_hit, saved_usd,
                         attribution_correlation_id, attribution_tags,
                         experiment_id, experiment_variant, tokens_estimated"#,
        )
        .bind(entry.user_id)
        .bind(entry.prompt_id)
        .bind(&entry.model)
        .bind(&entry.provider)
        .bind(&entry.project)
        .bind(entry.tokens_in)
        .bind(entry.tokens_out)
        .bind(entry.cost_usd)
        .bind(entry.api_key_id)
        .bind(&now)
        .bind(&entry.attribution_correlation_id)
        .bind(&entry.attribution_tags)
        .bind(entry.experiment_id)
        .bind(&entry.experiment_variant)
        .bind(entry.tokens_estimated)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn create_cache_hit(&self, entry: NewCostLedgerEntry) -> anyhow::Result<CostLedgerEntry> {
        let now = now_utc();
        // cost_usd is hard-coded to 0: a cache hit is usage, never spend.
        let row = sqlx::query_as::<_, CostLedgerEntry>(
            r#"INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project,
                                        tokens_in, tokens_out, cost_usd, api_key_id, created_at,
                                        cache_hit, saved_usd,
                                        attribution_correlation_id, attribution_tags,
                                        experiment_id, experiment_variant, tokens_estimated)
               VALUES ($1, $2, $3, $4, $5, $6, $7, 0.0, $8, $9, TRUE, $10, $11, $12, $13, $14, $15)
               RETURNING id, user_id, prompt_id, model, provider, project,
                         tokens_in, tokens_out, cost_usd, created_at, api_key_id,
                         cache_hit, saved_usd,
                         attribution_correlation_id, attribution_tags,
                         experiment_id, experiment_variant, tokens_estimated"#,
        )
        .bind(entry.user_id)
        .bind(entry.prompt_id)
        .bind(&entry.model)
        .bind(&entry.provider)
        .bind(&entry.project)
        .bind(entry.tokens_in)
        .bind(entry.tokens_out)
        .bind(entry.api_key_id)
        .bind(&now)
        .bind(entry.cost_usd)
        .bind(&entry.attribution_correlation_id)
        .bind(&entry.attribution_tags)
        .bind(entry.experiment_id)
        .bind(&entry.experiment_variant)
        .bind(entry.tokens_estimated)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn cache_summary_since(
        &self,
        filter_model: Option<&str>,
        since: &str,
    ) -> anyhow::Result<CacheUsageSummary> {
        let mut sql = format!(
            "SELECT {}, COUNT(*), COALESCE(SUM(saved_usd), 0.0) \
             FROM cost_ledger WHERE created_at >= $1",
            CACHE_HIT_SUM
        );
        if filter_model.is_some() {
            sql.push_str(" AND model = $2");
        }
        let mut q = sqlx::query_as::<_, (i64, i64, f64)>(&sql).bind(since);
        if let Some(m) = filter_model {
            q = q.bind(m.to_string());
        }
        let (hits, requests, saved_usd) = q.fetch_one(&self.pool).await?;
        Ok(CacheUsageSummary { hits, requests, saved_usd })
    }

    async fn cache_daily_series(
        &self,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<(String, i64, i64)>> {
        let sql = format!(
            "SELECT SUBSTRING(created_at FROM 1 FOR 10) AS day, {}, COUNT(*) \
             FROM cost_ledger WHERE created_at >= $1 AND created_at < $2 \
             GROUP BY day ORDER BY day ASC",
            CACHE_HIT_SUM
        );
        Ok(sqlx::query_as::<_, (String, i64, i64)>(&sql)
            .bind(start)
            .bind(end)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn cache_summary_by_model_since(
        &self,
        since: &str,
    ) -> anyhow::Result<Vec<(String, CacheUsageSummary)>> {
        let sql = format!(
            "SELECT model, {}, COUNT(*), COALESCE(SUM(saved_usd), 0.0) \
             FROM cost_ledger WHERE created_at >= $1 \
             GROUP BY model ORDER BY 2 DESC, model ASC",
            CACHE_HIT_SUM
        );
        let rows = sqlx::query_as::<_, (String, i64, i64, f64)>(&sql)
            .bind(since)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(model, hits, requests, saved_usd)| {
                (model, CacheUsageSummary { hits, requests, saved_usd })
            })
            .collect())
    }

    async fn cache_rows_grouped(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_api_key_id: Option<i64>,
        filter_model: Option<&str>,
        since: &str,
    ) -> anyhow::Result<Vec<(i64, String, Option<String>, Option<i64>, CacheUsageSummary)>> {
        if let Some(ids) = filter_user_ids {
            if ids.is_empty() {
                return Ok(vec![]);
            }
        }

        let mut param = 1usize;
        let mut sql = format!(
            "SELECT user_id, model, project, api_key_id, {}, COUNT(*), \
                    COALESCE(SUM(saved_usd), 0.0) \
             FROM cost_ledger WHERE created_at >= ${}",
            CACHE_HIT_SUM, param
        );
        param += 1;
        if filter_project.is_some() {
            sql.push_str(&format!(" AND project = ${}", param));
            param += 1;
        }
        if filter_api_key_id.is_some() {
            sql.push_str(&format!(" AND api_key_id = ${}", param));
            param += 1;
        }
        if let Some(ids) = filter_user_ids {
            let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND user_id IN ({})", list));
        }
        if filter_model.is_some() {
            sql.push_str(&format!(" AND model = ${}", param));
            param += 1;
        }
        let _ = param;
        sql.push_str(" GROUP BY user_id, model, project, api_key_id");

        type Row = (i64, String, Option<String>, Option<i64>, i64, i64, f64);
        let mut q = sqlx::query_as::<_, Row>(&sql).bind(since);
        if let Some(p) = filter_project {
            q = q.bind(p.to_string());
        }
        if let Some(k) = filter_api_key_id {
            q = q.bind(k);
        }
        if let Some(m) = filter_model {
            q = q.bind(m.to_string());
        }
        Ok(q
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|(user_id, model, project, key_id, hits, requests, saved_usd)| {
                (
                    user_id,
                    model,
                    project,
                    key_id,
                    CacheUsageSummary { hits, requests, saved_usd },
                )
            })
            .collect())
    }

    async fn sum_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger
             WHERE user_id = $1 AND created_at >= $2",
        )
        .bind(user_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn sum_tokens_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM cost_ledger
             WHERE user_id = $1 AND created_at >= $2",
        )
        .bind(user_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn sum_for_key_since(&self, api_key_id: i64, since: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger WHERE api_key_id = $1 AND created_at >= $2"
        )
        .bind(api_key_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn sum_tokens_for_key_since(&self, api_key_id: i64, since: &str) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM cost_ledger WHERE api_key_id = $1 AND created_at >= $2"
        )
        .bind(api_key_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn list_cost_entries_before(&self, _cutoff: &str) -> anyhow::Result<Vec<CostLedgerEntry>> {
        Ok(vec![])
    }

    async fn delete_cost_entries_by_ids(&self, _ids: &[i64]) -> anyhow::Result<()> {
        Ok(())
    }

    async fn sum_for_user_between(&self, user_id: i64, start: &str, end: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger \
             WHERE user_id = $1 AND created_at >= $2 AND created_at < $3"
        )
        .bind(user_id).bind(start).bind(end)
        .fetch_one(&self.pool).await?;
        Ok(row.0)
    }

    async fn sum_for_project_since(&self, project: &str, since: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger \
             WHERE project = $1 AND created_at >= $2"
        )
        .bind(project).bind(since)
        .fetch_one(&self.pool).await?;
        Ok(row.0)
    }

    async fn sum_for_project_between(&self, project: &str, start: &str, end: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger \
             WHERE project = $1 AND created_at >= $2 AND created_at < $3"
        )
        .bind(project).bind(start).bind(end)
        .fetch_one(&self.pool).await?;
        Ok(row.0)
    }

    async fn sum_global_since(&self, since: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger WHERE created_at >= $1"
        )
        .bind(since)
        .fetch_one(&self.pool).await?;
        Ok(row.0)
    }

    async fn sum_global_between(&self, start: &str, end: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger \
             WHERE created_at >= $1 AND created_at < $2"
        )
        .bind(start).bind(end)
        .fetch_one(&self.pool).await?;
        Ok(row.0)
    }

    async fn cost_stats_grouped(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_api_key_id: Option<i64>,
        since: &str,
    ) -> anyhow::Result<Vec<(i64, f64, i64, i64, i64)>> {
        if let Some(ids) = filter_user_ids {
            if ids.is_empty() {
                return Ok(vec![]);
            }
        }

        let mut param = 1usize;
        let mut sql = format!(
            "SELECT user_id, \
                    COALESCE(SUM(cost_usd), 0.0), \
                    COALESCE(SUM(tokens_in), 0), \
                    COALESCE(SUM(tokens_out), 0), \
                    COUNT(*) \
             FROM cost_ledger \
             WHERE created_at >= ${}", param
        );
        param += 1;

        if filter_project.is_some() {
            sql.push_str(&format!(" AND project = ${}", param));
            param += 1;
        }
        if filter_api_key_id.is_some() {
            sql.push_str(&format!(" AND api_key_id = ${}", param));
            param += 1;
        }
        let _ = param;
        if let Some(ids) = filter_user_ids {
            let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND user_id IN ({})", list));
        }
        sql.push_str(" GROUP BY user_id HAVING SUM(cost_usd) > 0 OR COUNT(*) > 0");

        let mut q = sqlx::query_as::<_, (i64, f64, i64, i64, i64)>(&sql);
        q = q.bind(since);
        if let Some(p) = filter_project {
            q = q.bind(p.to_string());
        }
        if let Some(k) = filter_api_key_id {
            q = q.bind(k);
        }
        Ok(q.fetch_all(&self.pool).await?)
    }

    async fn distinct_models_in_ledger(&self) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT model FROM cost_ledger WHERE model IS NOT NULL ORDER BY model",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(m,)| m).collect())
    }

    async fn distinct_providers_in_ledger(&self) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT provider FROM cost_ledger WHERE provider IS NOT NULL ORDER BY provider",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(p,)| p).collect())
    }

    async fn distinct_recent_correlation_ids(&self, limit: i64) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT attribution_correlation_id FROM cost_ledger \
             WHERE attribution_correlation_id IS NOT NULL AND attribution_correlation_id <> '' \
             GROUP BY attribution_correlation_id \
             ORDER BY MAX(created_at) DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn cost_rows_grouped(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_api_key_id: Option<i64>,
        filter_model: Option<&str>,
        since: &str,
    ) -> anyhow::Result<Vec<(i64, String, Option<String>, Option<i64>, f64, i64, i64, i64)>> {
        if let Some(ids) = filter_user_ids {
            if ids.is_empty() {
                return Ok(vec![]);
            }
        }

        let mut param = 1usize;
        let mut sql = format!(
            "SELECT user_id, model, project, api_key_id, \
                    COALESCE(SUM(cost_usd), 0.0), \
                    COALESCE(SUM(tokens_in), 0), \
                    COALESCE(SUM(tokens_out), 0), \
                    COUNT(*) \
             FROM cost_ledger \
             WHERE created_at >= ${}", param
        );
        param += 1;

        if filter_project.is_some() {
            sql.push_str(&format!(" AND project = ${}", param));
            param += 1;
        }
        if filter_api_key_id.is_some() {
            sql.push_str(&format!(" AND api_key_id = ${}", param));
            param += 1;
        }
        if let Some(ids) = filter_user_ids {
            let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND user_id IN ({})", list));
        }
        if filter_model.is_some() {
            sql.push_str(&format!(" AND model = ${}", param));
            param += 1;
        }
        let _ = param;
        sql.push_str(" GROUP BY user_id, model, project, api_key_id \
                       HAVING SUM(cost_usd) > 0 OR COUNT(*) > 0 \
                       ORDER BY SUM(cost_usd) DESC");

        type Row = (i64, String, Option<String>, Option<i64>, f64, i64, i64, i64);
        let mut q = sqlx::query_as::<_, Row>(&sql);
        q = q.bind(since);
        if let Some(p) = filter_project {
            q = q.bind(p.to_string());
        }
        if let Some(k) = filter_api_key_id {
            q = q.bind(k);
        }
        if let Some(m) = filter_model {
            q = q.bind(m.to_string());
        }
        Ok(q.fetch_all(&self.pool).await?)
    }

    async fn distinct_projects_in_ledger(&self) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT project FROM cost_ledger WHERE project IS NOT NULL ORDER BY project",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(p,)| p).collect())
    }

    async fn user_cost_stats_since(&self, user_id: i64, since: &str) -> anyhow::Result<(f64, i64, i64, i64)> {
        let row: (f64, i64, i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0),
                    COALESCE(SUM(tokens_in), 0),
                    COALESCE(SUM(tokens_out), 0),
                    COUNT(*)
             FROM cost_ledger
             WHERE user_id = $1 AND created_at >= $2",
        )
        .bind(user_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn run_stamp(&self, user_id: i64, correlation_id: &str) -> anyhow::Result<Option<RunStamp>> {
        // Stamped rows sort first, then the earliest wins; a run whose rows are
        // all unstamped therefore yields (NULL, NULL) rather than no row.
        let row: Option<(Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT experiment_id, experiment_variant FROM cost_ledger \
             WHERE user_id = $1 AND attribution_correlation_id = $2 \
             ORDER BY (experiment_id IS NULL), created_at, id LIMIT 1",
        )
        .bind(user_id)
        .bind(correlation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(experiment_id, experiment_variant)| RunStamp { experiment_id, experiment_variant }))
    }

    async fn list_daily_spend(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_model: Option<&str>,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<(String, f64)>> {
        if let Some(ids) = filter_user_ids {
            if ids.is_empty() {
                return Ok(vec![]);
            }
        }
        let mut param = 3usize;
        let mut sql = "SELECT to_char(created_at::date, 'YYYY-MM-DD') AS day, \
                              COALESCE(SUM(cost_usd), 0.0) \
                       FROM cost_ledger \
                       WHERE created_at >= $1 AND created_at < $2"
            .to_string();
        if filter_project.is_some() {
            sql.push_str(&format!(" AND project = ${}", param));
            param += 1;
        }
        if filter_model.is_some() {
            sql.push_str(&format!(" AND model = ${}", param));
            param += 1;
        }
        if let Some(ids) = filter_user_ids {
            let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND user_id IN ({})", list));
        }
        let _ = param;
        sql.push_str(" GROUP BY day ORDER BY day ASC");

        let mut q = sqlx::query_as::<_, (String, f64)>(&sql);
        q = q.bind(start).bind(end);
        if let Some(p) = filter_project { q = q.bind(p.to_string()); }
        if let Some(m) = filter_model { q = q.bind(m.to_string()); }
        Ok(q.fetch_all(&self.pool).await?)
    }

    async fn summarize_by_model(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_model: Option<&str>,
        since: &str,
    ) -> anyhow::Result<Vec<crate::db::repositories::costs::ModelSummaryRow>> {
        use crate::db::repositories::costs::ModelSummaryRow;
        if let Some(ids) = filter_user_ids {
            if ids.is_empty() {
                return Ok(vec![]);
            }
        }
        let mut param = 2usize;
        let mut sql = "SELECT model, \
                              COALESCE(SUM(cost_usd), 0.0), \
                              COALESCE(SUM(tokens_in), 0), \
                              COALESCE(SUM(tokens_out), 0), \
                              COUNT(*) \
                       FROM cost_ledger \
                       WHERE created_at >= $1"
            .to_string();
        if filter_project.is_some() {
            sql.push_str(&format!(" AND project = ${}", param));
            param += 1;
        }
        if filter_model.is_some() {
            sql.push_str(&format!(" AND model = ${}", param));
            param += 1;
        }
        if let Some(ids) = filter_user_ids {
            let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND user_id IN ({})", list));
        }
        let _ = param;
        sql.push_str(" GROUP BY model HAVING SUM(cost_usd) > 0 OR COUNT(*) > 0 ORDER BY SUM(cost_usd) DESC");

        let mut q = sqlx::query_as::<_, (String, f64, i64, i64, i64)>(&sql);
        q = q.bind(since);
        if let Some(p) = filter_project { q = q.bind(p.to_string()); }
        if let Some(m) = filter_model { q = q.bind(m.to_string()); }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(model, cost, ti, to, rc)| ModelSummaryRow {
            model,
            total_cost_usd: cost,
            tokens_in: ti,
            tokens_out: to,
            request_count: rc,
        }).collect())
    }

    // ── Attribution-filtered usage ────────────────────────────────────────────

    async fn attribution_totals(
        &self,
        filter: &AttributionFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<AttributionTotals> {
        let (predicate, binds) = attribution_predicate(filter);
        self.totals_where(&predicate, binds, start, end).await
    }

    async fn attribution_by_model(
        &self,
        filter: &AttributionFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>> {
        let (predicate, binds) = attribution_predicate(filter);
        self.by_model_where(&predicate, binds, start, end).await
    }

    async fn attribution_by_day(
        &self,
        filter: &AttributionFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>> {
        let (predicate, binds) = attribution_predicate(filter);
        self.by_day_where(&predicate, binds, start, end).await
    }

    async fn distinct_attribution_tag_keys(&self) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT jsonb_object_keys(attribution_tags::jsonb) AS k \
             FROM cost_ledger WHERE attribution_tags <> '{}' ORDER BY k ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(k,)| k).collect())
    }

    async fn distinct_attribution_values(
        &self,
        key: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<String>> {
        let rows = match key {
            None => {
                sqlx::query_as::<_, (String,)>(
                    "SELECT DISTINCT attribution_correlation_id AS v FROM cost_ledger \
                     WHERE attribution_correlation_id IS NOT NULL ORDER BY v ASC LIMIT $1",
                )
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            Some(k) => {
                sqlx::query_as::<_, (String,)>(
                    "SELECT DISTINCT (attribution_tags::jsonb ->> $1) AS v FROM cost_ledger \
                     WHERE (attribution_tags::jsonb ->> $1) IS NOT NULL ORDER BY v ASC LIMIT $2",
                )
                .bind(k.to_string())
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.into_iter().map(|(v,)| v).collect())
    }

    // ── Comparison arms ──────────────────────────────────────────────────────

    async fn arm_totals(
        &self,
        filter: &ArmFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<AttributionTotals> {
        let (predicate, binds) = arm_predicate(filter);
        self.totals_where(&predicate, binds, start, end).await
    }

    async fn arm_by_model(
        &self,
        filter: &ArmFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>> {
        let (predicate, binds) = arm_predicate(filter);
        self.by_model_where(&predicate, binds, start, end).await
    }

    async fn arm_by_day(
        &self,
        filter: &ArmFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>> {
        let (predicate, binds) = arm_predicate(filter);
        self.by_day_where(&predicate, binds, start, end).await
    }

    // ── Experiment results ───────────────────────────────────────────────────

    async fn experiment_variant_totals(
        &self,
        experiment_id: i64,
    ) -> anyhow::Result<Vec<ExperimentVariantTotals>> {
        let sql = format!(
            "SELECT COALESCE(experiment_variant, ''), {} FROM cost_ledger \
             WHERE experiment_id = $1 \
             GROUP BY experiment_variant ORDER BY experiment_variant ASC",
            EXPERIMENT_TOTALS_SELECT
        );
        let rows = sqlx::query_as::<_, ExperimentTotalsRow>(&sql)
            .bind(experiment_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(variant, requests, cost_usd, saved_usd, tokens_in, tokens_out, estimated_rows)| {
                ExperimentVariantTotals {
                    variant, requests, cost_usd, saved_usd, tokens_in, tokens_out, estimated_rows,
                }
            })
            .collect())
    }

    async fn experiment_variant_models(
        &self,
        experiment_id: i64,
    ) -> anyhow::Result<Vec<ExperimentVariantModelRow>> {
        let sql = format!(
            "SELECT COALESCE(experiment_variant, ''), model, {} FROM cost_ledger \
             WHERE experiment_id = $1 \
             GROUP BY experiment_variant, model \
             ORDER BY experiment_variant ASC, SUM(cost_usd) DESC, model ASC",
            EXPERIMENT_TOTALS_SELECT
        );
        type Row = (String, String, i64, f64, f64, i64, i64, i64);
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(experiment_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(variant, model, requests, cost_usd, saved_usd, tokens_in, tokens_out, estimated_rows)| {
                ExperimentVariantModelRow {
                    variant, model, requests, cost_usd, saved_usd, tokens_in, tokens_out, estimated_rows,
                }
            })
            .collect())
    }

    async fn experiment_runs(
        &self,
        experiment_id: i64,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<ExperimentRunRow>> {
        // `$1` is the experiment id for both the subquery and the outer
        // predicate; `$2`/`$3` are the page.
        let sql = format!(
            "SELECT user_id, attribution_correlation_id, {}, \
                    COUNT(DISTINCT experiment_variant), {}, \
                    MIN(created_at), MAX(created_at) AS last_at \
             FROM cost_ledger c \
             WHERE {} \
             GROUP BY user_id, attribution_correlation_id \
             ORDER BY last_at DESC, user_id ASC, attribution_correlation_id ASC \
             LIMIT $2 OFFSET $3",
            RUN_VARIANT_SUBQUERY, EXPERIMENT_TOTALS_SELECT, RUN_ROWS_WHERE
        );
        type Row = (i64, String, String, i64, i64, f64, f64, i64, i64, i64, String, String);
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(experiment_id)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    user_id, correlation_id, variant, variant_count, requests, cost_usd, saved_usd,
                    tokens_in, tokens_out, estimated_rows, first_at, last_at,
                )| ExperimentRunRow {
                    user_id, correlation_id, variant, variant_count, requests, cost_usd, saved_usd,
                    tokens_in, tokens_out, estimated_rows, first_at, last_at,
                },
            )
            .collect())
    }

    async fn experiment_run_keys(
        &self,
        experiment_id: i64,
    ) -> anyhow::Result<Vec<ExperimentRunKey>> {
        let sql = format!(
            "SELECT user_id, attribution_correlation_id, {}, COUNT(DISTINCT experiment_variant), \
                    COUNT(*), MIN(created_at), MAX(created_at) \
             FROM cost_ledger c WHERE {} \
             GROUP BY user_id, attribution_correlation_id \
             ORDER BY user_id ASC, attribution_correlation_id ASC",
            RUN_VARIANT_SUBQUERY, RUN_ROWS_WHERE
        );
        let rows = sqlx::query_as::<_, (i64, String, String, i64, i64, String, String)>(&sql)
            .bind(experiment_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(user_id, correlation_id, variant, n, requests, first_at, last_at)| {
                ExperimentRunKey {
                    user_id, correlation_id, variant, mixed: n > 1, requests, first_at, last_at,
                }
            })
            .collect())
    }

    async fn experiment_unbound_requests(
        &self,
        experiment_id: i64,
    ) -> anyhow::Result<Vec<ExperimentUnboundRow>> {
        let rows = sqlx::query_as::<_, (i64, String, i64)>(
            "SELECT u.user_id, u.attribution_correlation_id, COUNT(*) FROM cost_ledger u \
             WHERE u.experiment_id IS NULL AND u.attribution_correlation_id IS NOT NULL \
               AND EXISTS (SELECT 1 FROM cost_ledger b WHERE b.experiment_id = $1 \
                           AND b.user_id = u.user_id \
                           AND b.attribution_correlation_id = u.attribution_correlation_id) \
             GROUP BY u.user_id, u.attribution_correlation_id \
             ORDER BY u.user_id ASC, u.attribution_correlation_id ASC",
        )
        .bind(experiment_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(user_id, correlation_id, requests)| ExperimentUnboundRow {
                user_id, correlation_id, requests,
            })
            .collect())
    }
}

/// Aggregates over stamped rows; order matches [`ExperimentTotalsRow`] after
/// the grouping columns. `SUM` over BIGINT yields NUMERIC in Postgres, so the
/// token sums are cast back; `tokens_estimated` is BOOLEAN, hence the CASE.
const EXPERIMENT_TOTALS_SELECT: &str = "COUNT(*), COALESCE(SUM(cost_usd), 0.0), \
                                        COALESCE(SUM(saved_usd), 0.0), \
                                        COALESCE(SUM(tokens_in), 0)::BIGINT, \
                                        COALESCE(SUM(tokens_out), 0)::BIGINT, \
                                        COALESCE(SUM(CASE WHEN tokens_estimated THEN 1 ELSE 0 END), 0)::BIGINT";

/// Rows that form runs: stamped with the experiment (`$1`) and carrying a
/// correlation id. Aliased `c` so the variant subquery can correlate.
const RUN_ROWS_WHERE: &str = "c.experiment_id = $1 AND c.attribution_correlation_id IS NOT NULL";

/// The run's variant: its earliest stamped row by `created_at`, then id —
/// the rule `run_stamp` applies. A correlated subquery keeps the SQLite and
/// Postgres shapes identical; `$1` is the experiment id.
const RUN_VARIANT_SUBQUERY: &str =
    "(SELECT COALESCE(e.experiment_variant, '') FROM cost_ledger e \
      WHERE e.experiment_id = $1 AND e.user_id = c.user_id \
        AND e.attribution_correlation_id = c.attribution_correlation_id \
      ORDER BY e.created_at ASC, e.id ASC LIMIT 1)";

type ExperimentTotalsRow = (String, i64, f64, f64, i64, i64, i64);

impl PostgresDb {
    /// `SELECT totals FROM cost_ledger WHERE {predicate} AND window`. The
    /// predicate uses `$1..$n`; `start`/`end` take `$n+1`/`$n+2`.
    async fn totals_where(
        &self,
        predicate: &str,
        binds: Vec<String>,
        start: &str,
        end: &str,
    ) -> anyhow::Result<AttributionTotals> {
        let n = binds.len();
        let sql = format!(
            "SELECT {} FROM cost_ledger WHERE {} AND created_at >= ${} AND created_at < ${}",
            totals_select(),
            predicate,
            n + 1,
            n + 2
        );
        let mut q = sqlx::query_as::<_, TotalsRow>(&sql);
        for b in binds {
            q = q.bind(b);
        }
        let row = q.bind(start).bind(end).fetch_one(&self.pool).await?;
        Ok(totals_from_row(row))
    }

    async fn by_model_where(
        &self,
        predicate: &str,
        binds: Vec<String>,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>> {
        let n = binds.len();
        let sql = format!(
            "SELECT model, {} FROM cost_ledger \
             WHERE {} AND created_at >= ${} AND created_at < ${} \
             GROUP BY model ORDER BY SUM(cost_usd) DESC, model ASC",
            totals_select(),
            predicate,
            n + 1,
            n + 2
        );
        self.breakdown_query(&sql, binds, start, end).await
    }

    async fn by_day_where(
        &self,
        predicate: &str,
        binds: Vec<String>,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>> {
        let n = binds.len();
        // created_at is an ISO 8601 string column, so the first ten characters
        // are the calendar day — no timestamp cast needed.
        let sql = format!(
            "SELECT substring(created_at from 1 for 10) AS day, {} FROM cost_ledger \
             WHERE {} AND created_at >= ${} AND created_at < ${} \
             GROUP BY day ORDER BY day ASC",
            totals_select(),
            predicate,
            n + 1,
            n + 2
        );
        self.breakdown_query(&sql, binds, start, end).await
    }

    async fn breakdown_query(
        &self,
        sql: &str,
        binds: Vec<String>,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>> {
        let mut q = sqlx::query_as::<_, BreakdownRow>(sql);
        for b in binds {
            q = q.bind(b);
        }
        let rows = q.bind(start).bind(end).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(breakdown_from_row).collect())
    }
}

type TotalsRow = (f64, f64, i64, i64, i64, i64);
type BreakdownRow = (String, f64, f64, i64, i64, i64, i64);

/// Aggregate expression shared by every attribution query; order matches
/// [`TotalsRow`]. `cache_hit` is BOOLEAN here, hence the CASE.
fn totals_select() -> String {
    format!(
        "COALESCE(SUM(cost_usd), 0.0), COALESCE(SUM(saved_usd), 0.0), \
         COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0), COUNT(*), {}",
        CACHE_HIT_SUM
    )
}

fn totals_from_row(row: TotalsRow) -> AttributionTotals {
    let (cost_usd, saved_usd, tokens_in, tokens_out, requests, cache_hits) = row;
    AttributionTotals { cost_usd, saved_usd, tokens_in, tokens_out, requests, cache_hits }
}

fn breakdown_from_row(row: BreakdownRow) -> AttributionBreakdownRow {
    let (key, cost_usd, saved_usd, tokens_in, tokens_out, requests, cache_hits) = row;
    AttributionBreakdownRow {
        key,
        totals: AttributionTotals {
            cost_usd, saved_usd, tokens_in, tokens_out, requests, cache_hits,
        },
    }
}

/// SQL predicate plus its bound values, in placeholder order. Both the tag key
/// and its value are bound, so nothing from the caller reaches the SQL text.
pub(crate) fn attribution_predicate(filter: &AttributionFilter) -> (String, Vec<String>) {
    match filter {
        AttributionFilter::CorrelationId(v) => (
            "attribution_correlation_id = $1".to_string(),
            vec![v.clone()],
        ),
        AttributionFilter::Tag { key, value } => (
            "(attribution_tags::jsonb ->> $1) = $2".to_string(),
            vec![key.clone(), value.clone()],
        ),
    }
}

/// SQL predicate plus its bound values for one experiment variant. Shared by
/// the ledger, prompt and failure tables, which stamp the same two columns.
///
/// Binds are strings (sqlx declares them as TEXT), so the experiment id is
/// cast back to BIGINT in SQL.
pub(crate) fn variant_predicate(experiment_id: i64, variant: &str) -> (String, Vec<String>) {
    (
        "experiment_id = CAST($1 AS BIGINT) AND experiment_variant = $2".to_string(),
        vec![experiment_id.to_string(), variant.to_string()],
    )
}

/// SQL predicate plus its bound values for a comparison arm against the ledger.
fn arm_predicate(filter: &ArmFilter) -> (String, Vec<String>) {
    match filter {
        ArmFilter::Model(m) => ("model = $1".to_string(), vec![m.clone()]),
        ArmFilter::Provider(p) => ("provider = $1".to_string(), vec![p.clone()]),
        ArmFilter::Attribution(f) => attribution_predicate(f),
        ArmFilter::Variant { experiment_id, variant } => variant_predicate(*experiment_id, variant),
    }
}
