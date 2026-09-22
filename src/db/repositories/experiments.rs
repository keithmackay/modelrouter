use async_trait::async_trait;

use crate::db::models::{Experiment, ExperimentStatus, ExperimentVariants, NewExperiment};

/// Which experiments `list` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExperimentStatusFilter {
    All,
    Active,
    Closed,
}

/// Controlled experiments (spec §7a).
///
/// Rows are immutable after creation except for the `active -> closed`
/// transition, which happens once: an operator closes the experiment, or the
/// lifecycle tick closes it when `expires_at` has passed.
#[async_trait]
pub trait ExperimentRepository: Send + Sync {
    /// Errors when `name` is already taken.
    async fn create(&self, new: NewExperiment) -> anyhow::Result<Experiment>;
    async fn get(&self, id: i64) -> anyhow::Result<Option<Experiment>>;
    async fn list(&self, filter: ExperimentStatusFilter) -> anyhow::Result<Vec<Experiment>>;
    /// Close the experiment. Returns true when a row changed; only an active
    /// row does, so a second close is a no-op.
    async fn close(&self, id: i64, closed_at: &str) -> anyhow::Result<bool>;
    /// Close every active experiment whose `expires_at` is non-zero and not
    /// after `now_epoch`. Returns the ids closed.
    async fn close_expired(&self, now_epoch: i64, closed_at: &str) -> anyhow::Result<Vec<i64>>;
    /// Experiments with `retain_content` whose content window is still open:
    /// active, or closed with `content_retention_days = 0`, or closed less
    /// than `content_retention_days` before `now` (an RFC3339 timestamp). The
    /// window boundary is computed in Rust from `closed_at`, not in SQL.
    async fn all_retaining_open_or_within_window(&self, now: &str) -> anyhow::Result<Vec<Experiment>>;
    /// Closed experiments with `retain_content`, for the Rust-side check of
    /// whether their window has elapsed.
    async fn closed_retaining(&self) -> anyhow::Result<Vec<Experiment>>;
}

/// Row shape shared by both backends: JSON columns come back as text and are
/// parsed into the typed model in `Experiment::try_from`.
#[derive(sqlx::FromRow)]
pub struct ExperimentRow {
    pub id: i64,
    pub name: String,
    pub variants: String,
    pub allowed_user_ids: String,
    pub status: String,
    pub feed_learning: bool,
    pub expires_at: i64,
    pub created_at: String,
    pub closed_at: Option<String>,
    pub retain_content: bool,
    pub content_retention_days: i64,
}

impl TryFrom<ExperimentRow> for Experiment {
    type Error = anyhow::Error;

    fn try_from(r: ExperimentRow) -> anyhow::Result<Self> {
        let variants: ExperimentVariants = serde_json::from_str(&r.variants)?;
        let allowed_user_ids: Vec<i64> = serde_json::from_str(&r.allowed_user_ids)?;
        Ok(Experiment {
            id: r.id,
            name: r.name,
            variants,
            allowed_user_ids,
            status: ExperimentStatus::parse(&r.status)?,
            feed_learning: r.feed_learning,
            expires_at: r.expires_at,
            created_at: r.created_at,
            closed_at: r.closed_at,
            retain_content: r.retain_content,
            content_retention_days: r.content_retention_days,
        })
    }
}

/// Whether an experiment's content window is still open at `now`.
///
/// Shared by both backends so the boundary is defined once: active rows are
/// always open; closed rows are open forever when `content_retention_days`
/// is 0, and otherwise until `closed_at + content_retention_days`. A closed
/// row with an unparseable `closed_at` is treated as elapsed.
pub fn retention_window_open(exp: &Experiment, now: chrono::DateTime<chrono::Utc>) -> bool {
    if exp.status == ExperimentStatus::Active {
        return true;
    }
    if exp.content_retention_days == 0 {
        return true;
    }
    let Some(closed_at) = exp.closed_at.as_deref() else {
        return false;
    };
    let Ok(closed) = chrono::DateTime::parse_from_rfc3339(closed_at) else {
        return false;
    };
    let boundary = closed.with_timezone(&chrono::Utc)
        + chrono::Duration::days(exp.content_retention_days);
    now < boundary
}
