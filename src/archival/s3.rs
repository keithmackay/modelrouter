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

    pub async fn run_once(&self) -> anyhow::Result<usize> {
        use chrono::{Duration, Utc};
        let cutoff = Utc::now() - Duration::days(self.config.after_days as i64);
        let cutoff_str = cutoff.format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
        let rows = self.db.list_cost_entries_before(&cutoff_str).await?;
        if rows.is_empty() { return Ok(0); }
        let ndjson = super::rows_to_ndjson(&rows);
        let object_key = format!("{}/{}.ndjson", self.config.prefix, cutoff.format("%Y-%m-%d"));
        self.upload_ndjson(&object_key, ndjson).await?;
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
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
