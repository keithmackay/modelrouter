#[cfg(feature = "s3-archival")]
pub mod s3;

#[cfg(feature = "s3-archival")]
pub use s3::{ArchivalJob, spawn_archival_task};

use crate::db::models::CostLedgerEntry;

pub fn rows_to_ndjson(rows: &[CostLedgerEntry]) -> String {
    rows.iter()
        .map(|r| serde_json::to_string(r).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}
