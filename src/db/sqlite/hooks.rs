use async_trait::async_trait;

use crate::db::models::HookMetric;
use crate::db::repositories::hooks::HookRepository;
use super::{SqliteDb, now_utc};

#[derive(sqlx::FromRow)]
struct HookMetricRow {
    hook_name: String,
    invoked_at: String,
    duration_ms: i64,
    success: i64,
}

impl From<HookMetricRow> for HookMetric {
    fn from(r: HookMetricRow) -> Self {
        HookMetric {
            hook_name: r.hook_name,
            invoked_at: r.invoked_at,
            duration_ms: r.duration_ms,
            success: r.success != 0,
        }
    }
}

#[async_trait]
impl HookRepository for SqliteDb {
    async fn has_permission(&self, hook_name: &str, capability: &str) -> anyhow::Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM hook_permissions WHERE hook_name = ? AND capability = ? LIMIT 1",
        )
        .bind(hook_name)
        .bind(capability)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn grant_permission(
        &self,
        hook_name: &str,
        capability: &str,
        granted_by: Option<i64>,
    ) -> anyhow::Result<()> {
        let now = now_utc();
        sqlx::query(
            r#"INSERT OR IGNORE INTO hook_permissions (hook_name, capability, granted_by, granted_at)
               VALUES (?, ?, ?, ?)"#,
        )
        .bind(hook_name)
        .bind(capability)
        .bind(granted_by)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn revoke_permission(&self, hook_name: &str, capability: &str) -> anyhow::Result<()> {
        sqlx::query(
            "DELETE FROM hook_permissions WHERE hook_name = ? AND capability = ?",
        )
        .bind(hook_name)
        .bind(capability)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn record_metric(&self, metric: HookMetric) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO hook_metrics (hook_name, invoked_at, duration_ms, success)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&metric.hook_name)
        .bind(&metric.invoked_at)
        .bind(metric.duration_ms)
        .bind(metric.success as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_metrics_summary(&self, hook_name: &str) -> anyhow::Result<Vec<HookMetric>> {
        let rows = sqlx::query_as::<_, HookMetricRow>(
            "SELECT hook_name, invoked_at, duration_ms, success
             FROM hook_metrics WHERE hook_name = ? ORDER BY invoked_at DESC",
        )
        .bind(hook_name)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(HookMetric::from).collect())
    }
}
