use async_trait::async_trait;

use crate::db::models::{CostLedgerEntry, NewCostLedgerEntry, RunStamp};
use crate::db::repositories::costs::{
    ArmFilter, AttributionBreakdownRow, AttributionFilter, AttributionTotals,
    CacheUsageSummary, CostRepository, ExperimentRunRow, ExperimentRunKey,
    ExperimentUnboundRow, ExperimentVariantModelRow, ExperimentVariantTotals,
};
use super::{SqliteDb, now_utc};

/// Columns selected when reading a ledger row back.
const LEDGER_COLUMNS: &str = "id, user_id, prompt_id, model, provider, project, \
                              tokens_in, tokens_out, cost_usd, created_at, api_key_id, \
                              cache_hit, saved_usd, attribution_correlation_id, \
                              attribution_tags, experiment_id, experiment_variant, \
                              tokens_estimated";

#[async_trait]
impl CostRepository for SqliteDb {
    async fn create(&self, entry: NewCostLedgerEntry) -> anyhow::Result<CostLedgerEntry> {
        let now = now_utc();
        let result = sqlx::query(
            r#"INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project,
                                        tokens_in, tokens_out, cost_usd, api_key_id, created_at,
                                        attribution_correlation_id, attribution_tags,
                                        experiment_id, experiment_variant, tokens_estimated)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, CostLedgerEntry>(&format!(
            "SELECT {} FROM cost_ledger WHERE id = ?",
            LEDGER_COLUMNS
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn create_cache_hit(&self, entry: NewCostLedgerEntry) -> anyhow::Result<CostLedgerEntry> {
        let now = now_utc();
        // cost_usd is hard-coded to 0 here: a cache hit is usage, never spend.
        // The would-be cost travels in `saved_usd`.
        let result = sqlx::query(
            r#"INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project,
                                        tokens_in, tokens_out, cost_usd, api_key_id, created_at,
                                        cache_hit, saved_usd,
                                        attribution_correlation_id, attribution_tags,
                                        experiment_id, experiment_variant, tokens_estimated)
               VALUES (?, ?, ?, ?, ?, ?, ?, 0.0, ?, ?, 1, ?, ?, ?, ?, ?, ?)"#,
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
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, CostLedgerEntry>(&format!(
            "SELECT {} FROM cost_ledger WHERE id = ?",
            LEDGER_COLUMNS
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn cache_summary_since(
        &self,
        filter_model: Option<&str>,
        since: &str,
    ) -> anyhow::Result<CacheUsageSummary> {
        let mut sql = "SELECT COALESCE(SUM(cache_hit), 0), COUNT(*), COALESCE(SUM(saved_usd), 0.0) \
                       FROM cost_ledger WHERE created_at >= ?"
            .to_string();
        if filter_model.is_some() {
            sql.push_str(" AND model = ?");
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
        Ok(sqlx::query_as::<_, (String, i64, i64)>(
            "SELECT strftime('%Y-%m-%d', created_at) AS day, \
                    COALESCE(SUM(cache_hit), 0), COUNT(*) \
             FROM cost_ledger WHERE created_at >= ? AND created_at < ? \
             GROUP BY day ORDER BY day ASC",
        )
        .bind(start)
        .bind(end)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn cache_summary_by_model_since(
        &self,
        since: &str,
    ) -> anyhow::Result<Vec<(String, CacheUsageSummary)>> {
        let rows = sqlx::query_as::<_, (String, i64, i64, f64)>(
            "SELECT model, COALESCE(SUM(cache_hit), 0), COUNT(*), COALESCE(SUM(saved_usd), 0.0) \
             FROM cost_ledger WHERE created_at >= ? \
             GROUP BY model ORDER BY SUM(cache_hit) DESC, model ASC",
        )
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

        let mut sql = "SELECT user_id, model, project, api_key_id, \
                              COALESCE(SUM(cache_hit), 0), COUNT(*), \
                              COALESCE(SUM(saved_usd), 0.0) \
                       FROM cost_ledger WHERE created_at >= ?"
            .to_string();
        if filter_project.is_some() {
            sql.push_str(" AND project = ?");
        }
        if filter_api_key_id.is_some() {
            sql.push_str(" AND api_key_id = ?");
        }
        if let Some(ids) = filter_user_ids {
            let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND user_id IN ({})", list));
        }
        if filter_model.is_some() {
            sql.push_str(" AND model = ?");
        }
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
             WHERE user_id = ? AND created_at >= ?",
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
             WHERE user_id = ? AND created_at >= ?",
        )
        .bind(user_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn sum_for_key_since(&self, api_key_id: i64, since: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger WHERE api_key_id = ? AND created_at >= ?"
        )
        .bind(api_key_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn sum_tokens_for_key_since(&self, api_key_id: i64, since: &str) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM cost_ledger WHERE api_key_id = ? AND created_at >= ?"
        )
        .bind(api_key_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn list_cost_entries_before(&self, cutoff: &str) -> anyhow::Result<Vec<CostLedgerEntry>> {
        let rows = sqlx::query_as::<_, CostLedgerEntry>(&format!(
            "SELECT {} FROM cost_ledger WHERE created_at < ? ORDER BY created_at ASC LIMIT 10000",
            LEDGER_COLUMNS
        ))
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn delete_cost_entries_by_ids(&self, ids: &[i64]) -> anyhow::Result<()> {
        for chunk in ids.chunks(500) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("DELETE FROM cost_ledger WHERE id IN ({})", placeholders);
            let mut q = sqlx::query(&sql);
            for id in chunk { q = q.bind(id); }
            q.execute(&self.pool).await?;
        }
        Ok(())
    }

    async fn sum_for_user_between(&self, user_id: i64, start: &str, end: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger \
             WHERE user_id = ? AND created_at >= ? AND created_at < ?"
        )
        .bind(user_id).bind(start).bind(end)
        .fetch_one(&self.pool).await?;
        Ok(row.0)
    }

    async fn sum_for_project_since(&self, project: &str, since: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger \
             WHERE project = ? AND created_at >= ?"
        )
        .bind(project).bind(since)
        .fetch_one(&self.pool).await?;
        Ok(row.0)
    }

    async fn sum_for_project_between(&self, project: &str, start: &str, end: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger \
             WHERE project = ? AND created_at >= ? AND created_at < ?"
        )
        .bind(project).bind(start).bind(end)
        .fetch_one(&self.pool).await?;
        Ok(row.0)
    }

    async fn sum_global_since(&self, since: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger WHERE created_at >= ?"
        )
        .bind(since)
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

        let mut sql = "SELECT user_id, \
                              COALESCE(SUM(cost_usd), 0.0), \
                              COALESCE(SUM(tokens_in), 0), \
                              COALESCE(SUM(tokens_out), 0), \
                              COUNT(*) \
                       FROM cost_ledger \
                       WHERE created_at >= ?"
            .to_string();

        if filter_project.is_some() {
            sql.push_str(" AND project = ?");
        }
        if filter_api_key_id.is_some() {
            sql.push_str(" AND api_key_id = ?");
        }
        if let Some(ids) = filter_user_ids {
            // i64 values from our own DB — safe to inline
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
             WHERE user_id = ? AND created_at >= ?",
        )
        .bind(user_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
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
             WHERE attribution_correlation_id IS NOT NULL AND attribution_correlation_id != '' \
             GROUP BY attribution_correlation_id \
             ORDER BY MAX(created_at) DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn run_stamp(&self, user_id: i64, correlation_id: &str) -> anyhow::Result<Option<RunStamp>> {
        // Stamped rows sort first, then the earliest wins; a run whose rows are
        // all unstamped therefore yields (NULL, NULL) rather than no row.
        let row: Option<(Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT experiment_id, experiment_variant FROM cost_ledger \
             WHERE user_id = ? AND attribution_correlation_id = ? \
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
        let mut sql = "SELECT strftime('%Y-%m-%d', created_at) AS day, \
                              COALESCE(SUM(cost_usd), 0.0) \
                       FROM cost_ledger \
                       WHERE created_at >= ? AND created_at < ?"
            .to_string();
        if filter_project.is_some() {
            sql.push_str(" AND project = ?");
        }
        if filter_model.is_some() {
            sql.push_str(" AND model = ?");
        }
        if let Some(ids) = filter_user_ids {
            let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND user_id IN ({})", list));
        }
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
        let mut sql = "SELECT model, \
                              COALESCE(SUM(cost_usd), 0.0), \
                              COALESCE(SUM(tokens_in), 0), \
                              COALESCE(SUM(tokens_out), 0), \
                              COUNT(*) \
                       FROM cost_ledger \
                       WHERE created_at >= ?"
            .to_string();
        if filter_project.is_some() {
            sql.push_str(" AND project = ?");
        }
        if filter_model.is_some() {
            sql.push_str(" AND model = ?");
        }
        if let Some(ids) = filter_user_ids {
            let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND user_id IN ({})", list));
        }
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

        let mut sql = "SELECT user_id, model, project, api_key_id, \
                              COALESCE(SUM(cost_usd), 0.0), \
                              COALESCE(SUM(tokens_in), 0), \
                              COALESCE(SUM(tokens_out), 0), \
                              COUNT(*) \
                       FROM cost_ledger \
                       WHERE created_at >= ?"
            .to_string();

        if filter_project.is_some() {
            sql.push_str(" AND project = ?");
        }
        if filter_api_key_id.is_some() {
            sql.push_str(" AND api_key_id = ?");
        }
        if let Some(ids) = filter_user_ids {
            let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND user_id IN ({})", list));
        }
        if filter_model.is_some() {
            sql.push_str(" AND model = ?");
        }
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

    async fn sum_global_between(&self, start: &str, end: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger \
             WHERE created_at >= ? AND created_at < ?"
        )
        .bind(start).bind(end)
        .fetch_one(&self.pool).await?;
        Ok(row.0)
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
            "SELECT DISTINCT j.key FROM cost_ledger, json_each(cost_ledger.attribution_tags) AS j \
             WHERE cost_ledger.attribution_tags != '{}' ORDER BY j.key ASC",
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
                    "SELECT DISTINCT attribution_correlation_id FROM cost_ledger \
                     WHERE attribution_correlation_id IS NOT NULL \
                     ORDER BY attribution_correlation_id ASC LIMIT ?",
                )
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            Some(k) => {
                sqlx::query_as::<_, (String,)>(
                    "SELECT DISTINCT json_extract(attribution_tags, ?) AS v FROM cost_ledger \
                     WHERE json_extract(attribution_tags, ?) IS NOT NULL \
                     ORDER BY v ASC LIMIT ?",
                )
                .bind(AttributionFilter::tag_json_path(k))
                .bind(AttributionFilter::tag_json_path(k))
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
             WHERE experiment_id = ? \
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
             WHERE experiment_id = ? \
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
        let sql = format!(
            "SELECT user_id, attribution_correlation_id, {}, \
                    COUNT(DISTINCT experiment_variant), {}, \
                    MIN(created_at), MAX(created_at) AS last_at \
             FROM cost_ledger c \
             WHERE {} \
             GROUP BY user_id, attribution_correlation_id \
             ORDER BY last_at DESC, user_id ASC, attribution_correlation_id ASC \
             LIMIT ? OFFSET ?",
            RUN_VARIANT_SUBQUERY, EXPERIMENT_TOTALS_SELECT, RUN_ROWS_WHERE
        );
        type Row = (i64, String, String, i64, i64, f64, f64, i64, i64, i64, String, String);
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(experiment_id)
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

    async fn experiment_run_count(&self, experiment_id: i64) -> anyhow::Result<i64> {
        let sql = format!(
            "SELECT COUNT(*) FROM (SELECT 1 FROM cost_ledger c WHERE {} \
             GROUP BY user_id, attribution_correlation_id)",
            RUN_ROWS_WHERE
        );
        let (count,): (i64,) = sqlx::query_as(&sql)
            .bind(experiment_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
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
               AND EXISTS (SELECT 1 FROM cost_ledger b WHERE b.experiment_id = ? \
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
/// the grouping columns.
const EXPERIMENT_TOTALS_SELECT: &str = "COUNT(*), COALESCE(SUM(cost_usd), 0.0), \
                                        COALESCE(SUM(saved_usd), 0.0), \
                                        COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0), \
                                        COALESCE(SUM(tokens_estimated), 0)";

/// Rows that form runs: stamped with the experiment (first `?`) and carrying
/// a correlation id. Aliased `c` so the variant subquery can correlate.
const RUN_ROWS_WHERE: &str = "c.experiment_id = ? AND c.attribution_correlation_id IS NOT NULL";

/// The run's variant: its earliest stamped row by `created_at`, then id —
/// the rule `run_stamp` applies. A correlated subquery rather than SQLite's
/// bare-column-beside-MIN() shortcut: that shortcut follows the *last*
/// min()/max() in the select list, and the run query also takes
/// `MAX(created_at)`. Takes the experiment id as its own `?`.
const RUN_VARIANT_SUBQUERY: &str =
    "(SELECT COALESCE(e.experiment_variant, '') FROM cost_ledger e \
      WHERE e.experiment_id = ? AND e.user_id = c.user_id \
        AND e.attribution_correlation_id = c.attribution_correlation_id \
      ORDER BY e.created_at ASC, e.id ASC LIMIT 1)";

type ExperimentTotalsRow = (String, i64, f64, f64, i64, i64, i64);

impl SqliteDb {
    /// `SELECT totals FROM cost_ledger WHERE {predicate} AND window`, with
    /// `binds` bound in order before `start`/`end`.
    async fn totals_where(
        &self,
        predicate: &str,
        binds: Vec<String>,
        start: &str,
        end: &str,
    ) -> anyhow::Result<AttributionTotals> {
        let sql = format!(
            "SELECT {} FROM cost_ledger WHERE {} AND created_at >= ? AND created_at < ?",
            TOTALS_SELECT, predicate
        );
        let mut query = sqlx::query_as::<_, TotalsRow>(&sql);
        for b in binds {
            query = query.bind(b);
        }
        let row = query.bind(start).bind(end).fetch_one(&self.pool).await?;
        Ok(totals_from_row(row))
    }

    async fn by_model_where(
        &self,
        predicate: &str,
        binds: Vec<String>,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>> {
        let sql = format!(
            "SELECT model, {} FROM cost_ledger \
             WHERE {} AND created_at >= ? AND created_at < ? \
             GROUP BY model ORDER BY SUM(cost_usd) DESC, model ASC",
            TOTALS_SELECT, predicate
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
        let sql = format!(
            "SELECT strftime('%Y-%m-%d', created_at) AS day, {} FROM cost_ledger \
             WHERE {} AND created_at >= ? AND created_at < ? \
             GROUP BY day ORDER BY day ASC",
            TOTALS_SELECT, predicate
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
        let mut query = sqlx::query_as::<_, BreakdownRow>(sql);
        for b in binds {
            query = query.bind(b);
        }
        let rows = query.bind(start).bind(end).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(breakdown_from_row).collect())
    }
}

/// Aggregate expression shared by every attribution query. Order matches
/// [`TotalsRow`].
const TOTALS_SELECT: &str = "COALESCE(SUM(cost_usd), 0.0), COALESCE(SUM(saved_usd), 0.0), \
                             COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0), \
                             COUNT(*), COALESCE(SUM(cache_hit), 0)";

type TotalsRow = (f64, f64, i64, i64, i64, i64);
type BreakdownRow = (String, f64, f64, i64, i64, i64, i64);

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

/// SQL predicate plus its bound values, in `?` order, for an attribution filter.
///
/// The tag key becomes a JSON *path* that is bound as a parameter (like
/// `distinct_attribution_values`), so neither key nor value ever reaches the
/// SQL text.
pub(crate) fn attribution_predicate(filter: &AttributionFilter) -> (String, Vec<String>) {
    match filter {
        AttributionFilter::CorrelationId(v) => {
            ("attribution_correlation_id = ?".to_string(), vec![v.clone()])
        }
        AttributionFilter::Tag { key, value } => (
            "json_extract(attribution_tags, ?) = ?".to_string(),
            vec![AttributionFilter::tag_json_path(key), value.clone()],
        ),
    }
}

/// SQL predicate plus its bound values for a comparison arm against the ledger.
///
/// Binds are strings; the experiment id is cast back to an integer in SQL so
/// the comparison never relies on column affinity.
fn arm_predicate(filter: &ArmFilter) -> (String, Vec<String>) {
    match filter {
        ArmFilter::Model(m) => ("model = ?".to_string(), vec![m.clone()]),
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
    use crate::db::repositories::costs::{ArmFilter, CostRepository};
    use crate::db::sqlite::SqliteDb;

    async fn make_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    async fn insert_cost(db: &SqliteDb, project: Option<&str>, cost_usd: f64, created_at: &str) {
        let prompt_result = sqlx::query(
            "INSERT INTO prompts (user_id, session_id, request_model, routed_model, provider, \
             messages, response, finish_reason, prompt_tokens, completion_tokens, cost_usd, \
             latency_ms, tags, project, created_at) \
             VALUES (1, NULL, 'test', 'test', 'test', '[]', NULL, NULL, 0, 0, 0.0, NULL, '[]', ?, ?)"
        )
        .bind(project)
        .bind(created_at)
        .execute(&db.pool)
        .await
        .unwrap();
        let prompt_id = prompt_result.last_insert_rowid();
        sqlx::query(
            "INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project, \
             tokens_in, tokens_out, cost_usd, api_key_id, created_at) \
             VALUES (1, ?, 'test', 'test', ?, 0, 0, ?, NULL, ?)"
        )
        .bind(prompt_id)
        .bind(project)
        .bind(cost_usd)
        .bind(created_at)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sum_for_user_between_sums_in_range() {
        let db = make_db().await;
        sqlx::query("INSERT INTO users (id, name, enabled, created_at, metadata) VALUES (1, 'alice', 1, '2026-01-01T00:00:00Z', '{}')")
            .execute(&db.pool).await.unwrap();
        insert_cost(&db, None, 10.0, "2026-03-01T00:00:00Z").await;
        insert_cost(&db, None, 5.0,  "2026-03-15T00:00:00Z").await;
        insert_cost(&db, None, 20.0, "2026-04-01T00:00:00Z").await; // outside range

        let total = db.sum_for_user_between(1, "2026-03-01T00:00:00Z", "2026-04-01T00:00:00Z").await.unwrap();
        assert_eq!(total, 15.0);
    }

    #[tokio::test]
    async fn sum_for_project_since_sums_by_project() {
        let db = make_db().await;
        sqlx::query("INSERT INTO users (id, name, enabled, created_at, metadata) VALUES (1, 'alice', 1, '2026-01-01T00:00:00Z', '{}')")
            .execute(&db.pool).await.unwrap();
        insert_cost(&db, Some("billing"), 10.0, "2026-03-01T00:00:00Z").await;
        insert_cost(&db, Some("billing"), 5.0,  "2026-03-15T00:00:00Z").await;
        insert_cost(&db, Some("other"),   99.0, "2026-03-01T00:00:00Z").await;

        let total = db.sum_for_project_since("billing", "2026-01-01T00:00:00Z").await.unwrap();
        assert_eq!(total, 15.0);
    }

    #[tokio::test]
    async fn sum_for_project_between_filters_range() {
        let db = make_db().await;
        sqlx::query("INSERT INTO users (id, name, enabled, created_at, metadata) VALUES (1, 'alice', 1, '2026-01-01T00:00:00Z', '{}')")
            .execute(&db.pool).await.unwrap();
        insert_cost(&db, Some("billing"), 10.0, "2026-03-01T00:00:00Z").await;
        insert_cost(&db, Some("billing"), 5.0,  "2026-03-15T00:00:00Z").await;
        insert_cost(&db, Some("billing"), 20.0, "2026-04-01T00:00:00Z").await; // outside

        let total = db.sum_for_project_between("billing", "2026-03-01T00:00:00Z", "2026-04-01T00:00:00Z").await.unwrap();
        assert_eq!(total, 15.0);
    }

    #[tokio::test]
    async fn sum_global_since_sums_all() {
        let db = make_db().await;
        sqlx::query("INSERT INTO users (id, name, enabled, created_at, metadata) VALUES (1, 'alice', 1, '2026-01-01T00:00:00Z', '{}')")
            .execute(&db.pool).await.unwrap();
        insert_cost(&db, Some("billing"), 10.0, "2026-03-01T00:00:00Z").await;
        insert_cost(&db, Some("other"),   5.0,  "2026-03-15T00:00:00Z").await;
        insert_cost(&db, None,            3.0,  "2026-03-20T00:00:00Z").await;
        insert_cost(&db, None,            99.0, "2026-01-01T00:00:00Z").await; // before since

        let total = db.sum_global_since("2026-02-01T00:00:00Z").await.unwrap();
        assert_eq!(total, 18.0);
    }

    #[tokio::test]
    async fn sum_global_between_filters_range() {
        let db = make_db().await;
        sqlx::query("INSERT INTO users (id, name, enabled, created_at, metadata) VALUES (1, 'alice', 1, '2026-01-01T00:00:00Z', '{}')")
            .execute(&db.pool).await.unwrap();
        insert_cost(&db, None, 10.0, "2026-03-01T00:00:00Z").await;
        insert_cost(&db, None, 5.0,  "2026-03-15T00:00:00Z").await;
        insert_cost(&db, None, 99.0, "2026-04-01T00:00:00Z").await; // outside

        let total = db.sum_global_between("2026-03-01T00:00:00Z", "2026-04-01T00:00:00Z").await.unwrap();
        assert_eq!(total, 15.0);
    }

    // ---- comparison arms ----------------------------------------------------

    /// Ledger row shaped for the arm tests: no prompt row is needed because the
    /// arm queries never join. `run` / `tags` populate the attribution columns.
    #[allow(clippy::too_many_arguments)]
    async fn insert_arm_row(
        db: &SqliteDb,
        model: &str,
        provider: &str,
        run: Option<&str>,
        tags: &str,
        cache_hit: i64,
        cost_usd: f64,
        created_at: &str,
    ) {
        sqlx::query(
            "INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project, \
             tokens_in, tokens_out, cost_usd, saved_usd, cache_hit, api_key_id, \
             attribution_correlation_id, attribution_tags, created_at) \
             VALUES (1, NULL, ?, ?, NULL, 10, 5, ?, 0.0, ?, NULL, ?, ?, ?)",
        )
        .bind(model)
        .bind(provider)
        .bind(cost_usd)
        .bind(cache_hit)
        .bind(run)
        .bind(tags)
        .bind(created_at)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    async fn arm_db() -> SqliteDb {
        let db = make_db().await;
        sqlx::query("INSERT INTO users (id, name, enabled, created_at, metadata) VALUES (1, 'alice', 1, '2026-01-01T00:00:00Z', '{}')")
            .execute(&db.pool).await.unwrap();
        db
    }

    const W_START: &str = "2026-03-01T00:00:00Z";
    const W_END: &str = "2026-04-01T00:00:00Z";

    fn tag(key: &str, value: &str) -> ArmFilter {
        ArmFilter::Attribution(AttributionFilter::Tag { key: key.to_string(), value: value.to_string() })
    }

    fn run(id: &str) -> ArmFilter {
        ArmFilter::Attribution(AttributionFilter::CorrelationId(id.to_string()))
    }

    #[tokio::test]
    async fn arm_totals_by_model_counts_only_that_model() {
        let db = arm_db().await;
        insert_arm_row(&db, "X", "p1", None, "{}", 0, 1.0, "2026-03-02T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", None, "{}", 1, 0.0, "2026-03-03T00:00:00Z").await;
        insert_arm_row(&db, "X", "p2", None, "{}", 0, 2.0, "2026-03-04T00:00:00Z").await;
        insert_arm_row(&db, "Y", "p1", None, "{}", 0, 4.0, "2026-03-05T00:00:00Z").await;
        insert_arm_row(&db, "Y", "p2", None, "{}", 0, 8.0, "2026-03-06T00:00:00Z").await;

        let x = db.arm_totals(&ArmFilter::Model("X".into()), W_START, W_END).await.unwrap();
        assert_eq!(x.requests, 3);
        assert_eq!(x.cost_usd, 3.0);
        assert_eq!(x.cache_hits, 1);
        assert_eq!(x.tokens_in, 30);
        assert_eq!(x.tokens_out, 15);

        let y = db.arm_totals(&ArmFilter::Model("Y".into()), W_START, W_END).await.unwrap();
        assert_eq!(y.requests, 2);
        assert_eq!(y.cost_usd, 12.0);
        assert_eq!(y.cache_hits, 0);

        let none = db.arm_totals(&ArmFilter::Model("Z".into()), W_START, W_END).await.unwrap();
        assert_eq!(none, AttributionTotals::default());
    }

    #[tokio::test]
    async fn arm_totals_by_provider() {
        let db = arm_db().await;
        insert_arm_row(&db, "X", "p1", None, "{}", 0, 1.0, "2026-03-02T00:00:00Z").await;
        insert_arm_row(&db, "Y", "p1", None, "{}", 0, 2.0, "2026-03-03T00:00:00Z").await;
        insert_arm_row(&db, "X", "p2", None, "{}", 0, 4.0, "2026-03-04T00:00:00Z").await;

        let p1 = db.arm_totals(&ArmFilter::Provider("p1".into()), W_START, W_END).await.unwrap();
        assert_eq!(p1.requests, 2);
        assert_eq!(p1.cost_usd, 3.0);
        let p2 = db.arm_totals(&ArmFilter::Provider("p2".into()), W_START, W_END).await.unwrap();
        assert_eq!(p2.requests, 1);
        assert_eq!(p2.cost_usd, 4.0);
    }

    #[tokio::test]
    async fn arm_totals_by_tag_ignores_absent_and_different_values() {
        let db = arm_db().await;
        insert_arm_row(&db, "X", "p1", None, r#"{"arm":"a"}"#, 0, 1.0, "2026-03-02T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", None, r#"{"arm":"a","other":"z"}"#, 0, 2.0, "2026-03-03T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", None, r#"{"arm":"b"}"#, 0, 4.0, "2026-03-04T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", None, "{}", 0, 8.0, "2026-03-05T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", None, r#"{"other":"a"}"#, 0, 16.0, "2026-03-06T00:00:00Z").await;

        let a = db.arm_totals(&tag("arm", "a"), W_START, W_END).await.unwrap();
        assert_eq!(a.requests, 2);
        assert_eq!(a.cost_usd, 3.0);
        let b = db.arm_totals(&tag("arm", "b"), W_START, W_END).await.unwrap();
        assert_eq!(b.requests, 1);
        assert_eq!(b.cost_usd, 4.0);
    }

    #[tokio::test]
    async fn arm_totals_by_run_is_exact_not_prefix() {
        let db = arm_db().await;
        insert_arm_row(&db, "X", "p1", Some("run-1"), "{}", 0, 1.0, "2026-03-02T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", Some("run-10"), "{}", 0, 2.0, "2026-03-03T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", None, "{}", 0, 4.0, "2026-03-04T00:00:00Z").await;

        let r = db.arm_totals(&run("run-1"), W_START, W_END).await.unwrap();
        assert_eq!(r.requests, 1);
        assert_eq!(r.cost_usd, 1.0);
    }

    #[tokio::test]
    async fn arm_totals_window_is_half_open() {
        let db = arm_db().await;
        insert_arm_row(&db, "X", "p1", None, "{}", 0, 1.0, "2026-02-28T23:59:59Z").await; // one second before start
        insert_arm_row(&db, "X", "p1", None, "{}", 0, 2.0, "2026-03-01T00:00:00Z").await; // exactly at start
        insert_arm_row(&db, "X", "p1", None, "{}", 0, 4.0, "2026-03-31T23:59:59Z").await; // last second inside
        insert_arm_row(&db, "X", "p1", None, "{}", 0, 8.0, "2026-04-01T00:00:00Z").await; // exactly at end

        let t = db.arm_totals(&ArmFilter::Model("X".into()), W_START, W_END).await.unwrap();
        assert_eq!(t.requests, 2);
        assert_eq!(t.cost_usd, 6.0);
    }

    #[tokio::test]
    async fn arm_by_day_is_ascending_and_windowed() {
        let db = arm_db().await;
        insert_arm_row(&db, "X", "p1", None, "{}", 0, 1.0, "2026-03-10T05:00:00Z").await;
        insert_arm_row(&db, "X", "p1", None, "{}", 0, 2.0, "2026-03-10T18:00:00Z").await;
        insert_arm_row(&db, "X", "p1", None, "{}", 0, 4.0, "2026-03-02T00:00:00Z").await;
        insert_arm_row(&db, "Y", "p1", None, "{}", 0, 8.0, "2026-03-02T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", None, "{}", 0, 16.0, "2026-04-02T00:00:00Z").await; // outside

        let days = db.arm_by_day(&ArmFilter::Model("X".into()), W_START, W_END).await.unwrap();
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].key, "2026-03-02");
        assert_eq!(days[0].totals.cost_usd, 4.0);
        assert_eq!(days[0].totals.requests, 1);
        assert_eq!(days[1].key, "2026-03-10");
        assert_eq!(days[1].totals.cost_usd, 3.0);
        assert_eq!(days[1].totals.requests, 2);
    }

    #[tokio::test]
    async fn arm_by_model_orders_by_spend_desc() {
        let db = arm_db().await;
        insert_arm_row(&db, "X", "p1", Some("r"), "{}", 0, 1.0, "2026-03-02T00:00:00Z").await;
        insert_arm_row(&db, "Y", "p1", Some("r"), "{}", 0, 5.0, "2026-03-03T00:00:00Z").await;
        insert_arm_row(&db, "Y", "p1", Some("r"), "{}", 0, 5.0, "2026-03-04T00:00:00Z").await;
        insert_arm_row(&db, "Z", "p1", Some("other"), "{}", 0, 50.0, "2026-03-04T00:00:00Z").await;

        let rows = db.arm_by_model(&run("r"), W_START, W_END).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key, "Y");
        assert_eq!(rows[0].totals.cost_usd, 10.0);
        assert_eq!(rows[0].totals.requests, 2);
        assert_eq!(rows[1].key, "X");
        assert_eq!(rows[1].totals.cost_usd, 1.0);
    }

    #[tokio::test]
    async fn distinct_providers_in_ledger_is_sorted_and_deduped() {
        let db = arm_db().await;
        insert_arm_row(&db, "X", "zeta", None, "{}", 0, 1.0, "2026-03-02T00:00:00Z").await;
        insert_arm_row(&db, "X", "alpha", None, "{}", 0, 1.0, "2026-03-03T00:00:00Z").await;
        insert_arm_row(&db, "Y", "alpha", None, "{}", 0, 1.0, "2026-03-04T00:00:00Z").await;

        let providers = db.distinct_providers_in_ledger().await.unwrap();
        assert_eq!(providers, vec!["alpha".to_string(), "zeta".to_string()]);
    }

    #[tokio::test]
    async fn distinct_recent_correlation_ids_newest_first_and_capped() {
        let db = arm_db().await;
        insert_arm_row(&db, "X", "p1", Some("old"), "{}", 0, 1.0, "2026-03-01T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", Some("old"), "{}", 0, 1.0, "2026-03-02T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", Some("mid"), "{}", 0, 1.0, "2026-03-05T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", Some("new"), "{}", 0, 1.0, "2026-03-04T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", Some("new"), "{}", 0, 1.0, "2026-03-09T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", None, "{}", 0, 1.0, "2026-03-20T00:00:00Z").await;
        insert_arm_row(&db, "X", "p1", Some(""), "{}", 0, 1.0, "2026-03-21T00:00:00Z").await;

        let ids = db.distinct_recent_correlation_ids(2).await.unwrap();
        assert_eq!(ids, vec!["new".to_string(), "mid".to_string()]);

        let all = db.distinct_recent_correlation_ids(10).await.unwrap();
        assert_eq!(all, vec!["new".to_string(), "mid".to_string(), "old".to_string()]);
    }

    // ---- experiment stamps ------------------------------------------------

    async fn insert_stamped_row(
        db: &SqliteDb,
        user_id: i64,
        run: &str,
        experiment: Option<(i64, &str)>,
        created_at: &str,
    ) {
        sqlx::query(
            "INSERT INTO cost_ledger (user_id, prompt_id, model, provider, tokens_in, tokens_out, \
             cost_usd, attribution_correlation_id, experiment_id, experiment_variant, created_at) \
             VALUES (?, NULL, 'm', 'p', 1, 1, 0.0, ?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(run)
        .bind(experiment.map(|(id, _)| id))
        .bind(experiment.map(|(_, v)| v))
        .bind(created_at)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    async fn stamp_db() -> SqliteDb {
        let db = arm_db().await;
        sqlx::query("INSERT INTO users (id, name, enabled, created_at, metadata) VALUES (2, 'bob', 1, '2026-01-01T00:00:00Z', '{}')")
            .execute(&db.pool).await.unwrap();
        db
    }

    #[tokio::test]
    async fn run_stamp_is_the_earliest_stamped_row() {
        let db = stamp_db().await;
        // An unstamped row first, then two stamped rows out of insertion order.
        insert_stamped_row(&db, 1, "run-1", None, "2026-03-01T00:00:00Z").await;
        insert_stamped_row(&db, 1, "run-1", Some((7, "candidate")), "2026-03-03T00:00:00Z").await;
        insert_stamped_row(&db, 1, "run-1", Some((7, "control")), "2026-03-02T00:00:00Z").await;
        // Same correlation id under another user must not leak across.
        insert_stamped_row(&db, 2, "run-1", Some((8, "other")), "2026-03-01T00:00:00Z").await;

        let stamp = db.run_stamp(1, "run-1").await.unwrap();
        assert_eq!(
            stamp,
            Some(RunStamp { experiment_id: Some(7), experiment_variant: Some("control".into()) })
        );
        let other = db.run_stamp(2, "run-1").await.unwrap();
        assert_eq!(other.unwrap().experiment_id, Some(8));
    }

    #[tokio::test]
    async fn run_stamp_ties_break_on_id() {
        let db = stamp_db().await;
        insert_stamped_row(&db, 1, "run-1", Some((7, "first")), "2026-03-01T00:00:00Z").await;
        insert_stamped_row(&db, 1, "run-1", Some((7, "second")), "2026-03-01T00:00:00Z").await;
        let stamp = db.run_stamp(1, "run-1").await.unwrap().unwrap();
        assert_eq!(stamp.experiment_variant.as_deref(), Some("first"));
    }

    #[tokio::test]
    async fn run_stamp_unstamped_rows_and_no_rows() {
        let db = stamp_db().await;
        insert_stamped_row(&db, 1, "run-1", None, "2026-03-01T00:00:00Z").await;
        assert_eq!(db.run_stamp(1, "run-1").await.unwrap(), Some(RunStamp::default()));
        assert_eq!(db.run_stamp(1, "run-2").await.unwrap(), None);
        assert_eq!(db.run_stamp(2, "run-1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn create_writes_and_reads_back_experiment_columns() {
        let db = arm_db().await;
        let entry = NewCostLedgerEntry {
            user_id: 1,
            prompt_id: None,
            model: "m".into(),
            provider: "p".into(),
            project: None,
            tokens_in: 1,
            tokens_out: 1,
            cost_usd: 0.1,
            api_key_id: None,
            attribution_correlation_id: Some("run-1".into()),
            attribution_tags: "{}".into(),
            experiment_id: Some(4),
            experiment_variant: Some("control".into()),
            tokens_estimated: true,
        };
        let row = db.create(entry.clone()).await.unwrap();
        assert_eq!(row.experiment_id, Some(4));
        assert_eq!(row.experiment_variant.as_deref(), Some("control"));
        assert!(row.tokens_estimated);
        let hit = db.create_cache_hit(entry).await.unwrap();
        assert_eq!(hit.experiment_id, Some(4));
        assert!(hit.cache_hit);
        assert!(hit.tokens_estimated);
    }

    /// Stamped row with explicit figures for the experiment aggregates.
    #[allow(clippy::too_many_arguments)]
    async fn insert_experiment_row(
        db: &SqliteDb,
        user_id: i64,
        run: &str,
        experiment: Option<(i64, &str)>,
        model: &str,
        cost: f64,
        tokens: (i64, i64),
        estimated: bool,
        created_at: &str,
    ) {
        sqlx::query(
            "INSERT INTO cost_ledger (user_id, prompt_id, model, provider, tokens_in, tokens_out, \
             cost_usd, saved_usd, attribution_correlation_id, experiment_id, experiment_variant, \
             tokens_estimated, created_at) \
             VALUES (?, NULL, ?, 'p', ?, ?, ?, 0.0, ?, ?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(model)
        .bind(tokens.0)
        .bind(tokens.1)
        .bind(cost)
        .bind(run)
        .bind(experiment.map(|(id, _)| id))
        .bind(experiment.map(|(_, v)| v))
        .bind(estimated)
        .bind(created_at)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    /// Experiment 7: run `a` mixed (control first, then candidate), run `b`
    /// candidate with an estimated row and an unbound turn, run `b` under
    /// user 2 as a separate run, and unrelated rows for experiment 8.
    async fn experiment_db() -> SqliteDb {
        let db = stamp_db().await;
        let rows = [
            (1, "a", Some((7, "control")), "m1", 0.01, (10, 5), false, "2026-03-02T00:00:00Z"),
            (1, "a", Some((7, "candidate")), "m2", 0.02, (20, 10), false, "2026-03-03T00:00:00Z"),
            (1, "b", Some((7, "candidate")), "m2", 0.04, (40, 20), true, "2026-03-04T00:00:00Z"),
            (1, "b", None, "m2", 9.0, (900, 900), false, "2026-03-05T00:00:00Z"),
            (2, "b", Some((7, "control")), "m1", 0.08, (80, 40), false, "2026-03-01T00:00:00Z"),
            (1, "z", Some((8, "control")), "m1", 5.0, (1, 1), false, "2026-03-06T00:00:00Z"),
            (1, "none", None, "m1", 5.0, (1, 1), false, "2026-03-06T00:00:00Z"),
        ];
        for (user, run, experiment, model, cost, tokens, estimated, at) in rows {
            let experiment: Option<(i64, &str)> = experiment;
            insert_experiment_row(&db, user, run, experiment, model, cost, tokens, estimated, at)
                .await;
        }
        db
    }

    #[tokio::test]
    async fn experiment_variant_totals_follow_each_rows_variant() {
        let db = experiment_db().await;
        let totals = db.experiment_variant_totals(7).await.unwrap();
        let labels: Vec<&str> = totals.iter().map(|t| t.variant.as_str()).collect();
        assert_eq!(labels, ["candidate", "control"]);
        let candidate = &totals[0];
        assert_eq!(candidate.requests, 2);
        assert!((candidate.cost_usd - 0.06).abs() < 1e-9);
        assert_eq!((candidate.tokens_in, candidate.tokens_out), (60, 30));
        assert_eq!(candidate.estimated_rows, 1);
        let control = &totals[1];
        assert_eq!(control.requests, 2);
        assert!((control.cost_usd - 0.09).abs() < 1e-9);
        assert_eq!(control.estimated_rows, 0);
        assert!(db.experiment_variant_totals(9).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn experiment_variant_models_split_by_model() {
        let db = experiment_db().await;
        let rows = db.experiment_variant_models(7).await.unwrap();
        let keys: Vec<(&str, &str, i64)> = rows
            .iter()
            .map(|r| (r.variant.as_str(), r.model.as_str(), r.requests))
            .collect();
        assert_eq!(keys, [("candidate", "m2", 2), ("control", "m1", 2)]);
        assert_eq!(rows[0].estimated_rows, 1);
        assert_eq!(rows[0].tokens_in, 60);
    }

    #[tokio::test]
    async fn experiment_runs_take_the_earliest_variant_and_flag_mixed() {
        let db = experiment_db().await;
        assert_eq!(db.experiment_run_count(7).await.unwrap(), 3);
        assert_eq!(db.experiment_run_count(9).await.unwrap(), 0);

        let runs = db.experiment_runs(7, 10, 0).await.unwrap();
        let order: Vec<(i64, &str)> = runs.iter().map(|r| (r.user_id, r.correlation_id.as_str())).collect();
        assert_eq!(order, [(1, "b"), (1, "a"), (2, "b")], "last stamped activity first");

        let a = &runs[1];
        assert_eq!(a.variant, "control", "earliest stamped row wins");
        assert!(a.mixed());
        assert_eq!(a.variant_count, 2);
        assert_eq!(a.requests, 2);
        assert!((a.cost_usd - 0.03).abs() < 1e-9);
        assert_eq!((a.first_at.as_str(), a.last_at.as_str()), ("2026-03-02T00:00:00Z", "2026-03-03T00:00:00Z"));

        let b = &runs[0];
        assert_eq!(b.variant, "candidate");
        assert!(!b.mixed());
        assert_eq!(b.requests, 1, "the unbound turn is not a stamped request");
        assert!((b.cost_usd - 0.04).abs() < 1e-9);
        assert_eq!(b.estimated_rows, 1);
        assert_eq!(b.last_at, "2026-03-04T00:00:00Z", "the unbound turn does not extend the span");

        let page = db.experiment_runs(7, 1, 1).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].correlation_id, "a");

        let keys = db.experiment_run_keys(7).await.unwrap();
        let summary: Vec<(i64, &str, &str, bool, i64)> = keys
            .iter()
            .map(|k| (k.user_id, k.correlation_id.as_str(), k.variant.as_str(), k.mixed, k.requests))
            .collect();
        assert_eq!(
            summary,
            [(1, "a", "control", true, 2), (1, "b", "candidate", false, 1), (2, "b", "control", false, 1)]
        );
    }

    #[tokio::test]
    async fn experiment_unbound_requests_share_a_bound_runs_key() {
        let db = experiment_db().await;
        let unbound = db.experiment_unbound_requests(7).await.unwrap();
        let rows: Vec<(i64, &str, i64)> = unbound
            .iter()
            .map(|u| (u.user_id, u.correlation_id.as_str(), u.requests))
            .collect();
        assert_eq!(rows, [(1, "b", 1)], "run `none` has no bound row and is not counted");
        assert!(db.experiment_unbound_requests(8).await.unwrap().is_empty());
    }
}
