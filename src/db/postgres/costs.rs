#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::{CostLedgerEntry, NewCostLedgerEntry};
use crate::db::repositories::costs::{CacheUsageSummary, CostRepository};
use super::{PostgresDb, now_utc};

/// Postgres stores `cache_hit` as BOOLEAN, so it must be cast before SUM.
const CACHE_HIT_SUM: &str = "COALESCE(SUM(CASE WHEN cache_hit THEN 1 ELSE 0 END), 0)";

#[async_trait]
impl CostRepository for PostgresDb {
    async fn create(&self, entry: NewCostLedgerEntry) -> anyhow::Result<CostLedgerEntry> {
        let now = now_utc();
        let row = sqlx::query_as::<_, CostLedgerEntry>(
            r#"INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project,
                                        tokens_in, tokens_out, cost_usd, api_key_id, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
               RETURNING id, user_id, prompt_id, model, provider, project,
                         tokens_in, tokens_out, cost_usd, created_at, api_key_id,
                         cache_hit, saved_usd"#,
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
                                        cache_hit, saved_usd)
               VALUES ($1, $2, $3, $4, $5, $6, $7, 0.0, $8, $9, TRUE, $10)
               RETURNING id, user_id, prompt_id, model, provider, project,
                         tokens_in, tokens_out, cost_usd, created_at, api_key_id,
                         cache_hit, saved_usd"#,
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
}
