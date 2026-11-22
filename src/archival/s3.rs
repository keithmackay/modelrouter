use crate::config::schema::ArchivalConfig;
use crate::api::app::DatabaseProvider;
use std::sync::Arc;

pub struct ArchivalJob {
    config: ArchivalConfig,
    db: Arc<dyn DatabaseProvider>,
}

impl ArchivalJob {
    pub fn new(config: ArchivalConfig, db: Arc<dyn DatabaseProvider>) -> Self {
        Self { config, db }
    }

    /// Archive and delete the rows older than the cutoff, on whichever one
    /// replica wins the job lock; the others skip the pass. `Ok(0)` when
    /// there was nothing to do or another replica is doing it.
    pub async fn run_once(&self) -> anyhow::Result<usize> {
        let db = self.db.clone();
        crate::db::repositories::maintenance::with_job_lock(&*db, "s3-archival", self.archive_once())
            .await
            .unwrap_or(Ok(0))
    }

    async fn archive_once(&self) -> anyhow::Result<usize> {
        use chrono::{Duration, Utc};
        let cutoff = Utc::now() - Duration::days(self.config.after_days as i64);
        let cutoff_str = cutoff.format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
        let rows = self.db.list_cost_entries_before(&cutoff_str).await?;
        if rows.is_empty() { return Ok(0); }
        let ndjson = super::rows_to_ndjson(&rows);
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let object_key = object_key(&self.config.prefix, cutoff, &ids);
        self.upload_ndjson(&object_key, ndjson).await?;
        self.db.delete_cost_entries_by_ids(&ids).await?;
        Ok(ids.len())
    }

    async fn upload_ndjson(&self, key: &str, content: String) -> anyhow::Result<()> {
        let url = format!("{}/{}/{}", self.config.endpoint, self.config.bucket, key);
        let client = reqwest::Client::new();
        let resp = client.put(&url)
            .header("Content-Type", "application/x-ndjson")
            .body(content)
            .send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("S3 upload failed: {}", resp.status());
        }
        Ok(())
    }
}

/// One object per pass, never reused: the cutoff date for browsing, then the
/// id range it holds. A date-only key let a second pass on the same day (the
/// job runs every six hours) overwrite the first pass's object after its
/// rows had already been deleted from the ledger.
fn object_key(prefix: &str, cutoff: chrono::DateTime<chrono::Utc>, ids: &[i64]) -> String {
    let first = ids.iter().copied().min().unwrap_or(0);
    let last = ids.iter().copied().max().unwrap_or(0);
    format!("{}/{}/{first}-{last}.ndjson", prefix, cutoff.format("%Y-%m-%d"))
}

pub fn spawn_archival_task(job: ArchivalJob) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = job.run_once().await {
                tracing::warn!("archival job failed: {e}");
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(6 * 3600)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::object_key;
    use chrono::TimeZone;

    #[test]
    fn two_passes_on_one_day_get_different_objects() {
        let day = chrono::Utc.with_ymd_and_hms(2026, 11, 1, 6, 0, 0).unwrap();
        let later = chrono::Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap();
        let a = object_key("ledger", day, &[3, 1, 2]);
        let b = object_key("ledger", later, &[4, 5]);
        assert_eq!(a, "ledger/2026-11-01/1-3.ndjson");
        assert_eq!(b, "ledger/2026-11-01/4-5.ndjson");
        assert_ne!(a, b);
    }
}
