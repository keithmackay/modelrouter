use async_trait::async_trait;

use crate::db::models::{CostLedgerEntry, NewCostLedgerEntry};
use crate::db::repositories::costs::{
    ArmFilter, AttributionBreakdownRow, AttributionFilter, AttributionTotals,
    CacheUsageSummary, CostRepository,
};
use super::{SqliteDb, now_utc};

/// Columns selected when reading a ledger row back.
const LEDGER_COLUMNS: &str = "id, user_id, prompt_id, model, provider, project, \
                              tokens_in, tokens_out, cost_usd, created_at, api_key_id, \
                              cache_hit, saved_usd, attribution_correlation_id, \
                              attribution_tags";

#[async_trait]
impl CostRepository for SqliteDb {
    async fn create(&self, entry: NewCostLedgerEntry) -> anyhow::Result<CostLedgerEntry> {
        let now = now_utc();
        let result = sqlx::query(
            r#"INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project,
                                        tokens_in, tokens_out, cost_usd, api_key_id, created_at,
                                        attribution_correlation_id, attribution_tags)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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
                                        attribution_correlation_id, attribution_tags)
               VALUES (?, ?, ?, ?, ?, ?, ?, 0.0, ?, ?, 1, ?, ?, ?)"#,
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
        let (predicate, binds) = arm_predicate(filter, "model");
        self.totals_where(&predicate, binds, start, end).await
    }

    async fn arm_by_model(
        &self,
        filter: &ArmFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>> {
        let (predicate, binds) = arm_predicate(filter, "model");
        self.by_model_where(&predicate, binds, start, end).await
    }

    async fn arm_by_day(
        &self,
        filter: &ArmFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>> {
        let (predicate, binds) = arm_predicate(filter, "model");
        self.by_day_where(&predicate, binds, start, end).await
    }
}

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

/// SQL predicate plus its bound values for a comparison arm. `model_column`
/// is the expression a model arm matches against — each table names the
/// served model differently (`model` here, `routed_model` in `prompts`,
/// `COALESCE(routed_model, request_model)` in `request_failures`).
pub(crate) fn arm_predicate(filter: &ArmFilter, model_column: &str) -> (String, Vec<String>) {
    match filter {
        ArmFilter::Model(m) => (format!("{} = ?", model_column), vec![m.clone()]),
        ArmFilter::Provider(p) => ("provider = ?".to_string(), vec![p.clone()]),
        ArmFilter::Attribution(f) => attribution_predicate(f),
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
}
