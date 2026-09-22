//! Prompt-log retention: the hourly tick behind `serve` (spec §7c).
//!
//! Two halves, both run by [`run_retention_tick`] on every tick:
//!
//! - the global sweep — `purge_older_than_except` on the prompt store with the
//!   ids of every retaining experiment whose content window is still open, so
//!   an operator's `prompt_retention_days` never deletes an experiment's
//!   evidence early. It only runs when `retention_days > 0`, and it is skipped
//!   for the tick when the experiment list cannot be read: running it with an
//!   empty exception list would be the one outcome the exception list exists
//!   to prevent.
//! - the experiment half — for every closed retaining experiment whose
//!   `closed_at + content_retention_days` has passed (computed in Rust from
//!   the fetched rows, never in dialect-specific SQL), the content columns
//!   are redacted in place to the shape a non-retaining row has and the
//!   feedback notes of its runs are cleared. Latency, tokens, ids and stamps
//!   survive, so the results page reads the same as for any other closed
//!   experiment. A window of `0` days means never. This half runs regardless
//!   of `retention_days`, and one failing experiment does not stop the rest.
//!
//! The experiment list and the outcomes live in the main database; the
//! prompt rows live in the prompt store, which is the same database unless
//! `[storage] prompt_db_path` is set. The tick takes the two separately so
//! that split is honoured.

use crate::db::repositories::experiments::{retention_window_open, ExperimentRepository};
use crate::db::repositories::outcomes::OutcomeRepository;
use crate::db::repositories::prompts::PromptRepository;

/// How the global sweep of one tick ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepOutcome {
    /// `retention_days == 0`: deletion is opt-in and was not asked for.
    Disabled,
    /// The experiment list could not be read, so nothing was deleted.
    SkippedUnreadableExperiments(String),
    /// The sweep ran and deleted this many rows, protecting the listed
    /// experiments.
    Purged { deleted: u64, protected: Vec<i64> },
    /// The sweep itself failed.
    Failed(String),
}

/// What one tick did to one elapsed experiment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredExperiment {
    pub experiment_id: i64,
    /// Rows whose content was redacted by this tick (0 once already redacted).
    pub redacted: u64,
    /// Feedback notes cleared by this tick.
    pub notes_cleared: u64,
}

/// Everything one tick did, for the log line and for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionReport {
    pub sweep: SweepOutcome,
    /// Elapsed experiments handled this tick, in id order.
    pub expired: Vec<ExpiredExperiment>,
    /// Per-step failures on the experiment half, as `(experiment id, error)`;
    /// an id may appear once for the redaction and once for the notes. A
    /// failure to read the closed list is recorded under id `0`.
    pub failures: Vec<(i64, String)>,
}

/// Run one retention tick at `now`.
///
/// `experiments` and `outcomes` are the main database; `prompts` is the
/// prompt store (the same database, or the dedicated file). Never panics and
/// never returns an error: every failure is reported in the result and
/// logged, because the hourly loop must keep going.
pub async fn run_retention_tick(
    experiments: &dyn ExperimentRepository,
    outcomes: &dyn OutcomeRepository,
    prompts: &dyn PromptRepository,
    retention_days: u64,
    now: chrono::DateTime<chrono::Utc>,
) -> RetentionReport {
    let sweep = global_sweep(experiments, prompts, retention_days, now).await;
    let (expired, failures) = expire_closed(experiments, outcomes, prompts, now).await;
    RetentionReport { sweep, expired, failures }
}

async fn global_sweep(
    experiments: &dyn ExperimentRepository,
    prompts: &dyn PromptRepository,
    retention_days: u64,
    now: chrono::DateTime<chrono::Utc>,
) -> SweepOutcome {
    if retention_days == 0 {
        return SweepOutcome::Disabled;
    }
    let protected: Vec<i64> = match experiments
        .all_retaining_open_or_within_window(&now.to_rfc3339())
        .await
    {
        Ok(open) => open.into_iter().map(|e| e.id).collect(),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "prompt-log retention: experiment list unreadable, skipping the global purge this tick"
            );
            return SweepOutcome::SkippedUnreadableExperiments(e.to_string());
        }
    };
    // An absurd retention value must not panic the loop: a span chrono cannot
    // represent is the same as "keep everything".
    let Some(span) = i64::try_from(retention_days)
        .ok()
        .and_then(chrono::Duration::try_days)
        .and_then(|d| now.checked_sub_signed(d))
    else {
        return SweepOutcome::Disabled;
    };
    let cutoff = span.to_rfc3339();
    match prompts.purge_older_than_except(&cutoff, &protected).await {
        Ok(deleted) => {
            if deleted > 0 {
                tracing::info!(
                    deleted,
                    retention_days,
                    protected_experiments = protected.len(),
                    "prompt-log retention purge"
                );
            }
            SweepOutcome::Purged { deleted, protected }
        }
        Err(e) => {
            tracing::warn!(error = %e, "prompt-log retention purge failed");
            SweepOutcome::Failed(e.to_string())
        }
    }
}

async fn expire_closed(
    experiments: &dyn ExperimentRepository,
    outcomes: &dyn OutcomeRepository,
    prompts: &dyn PromptRepository,
    now: chrono::DateTime<chrono::Utc>,
) -> (Vec<ExpiredExperiment>, Vec<(i64, String)>) {
    let mut expired = Vec::new();
    let mut failures = Vec::new();
    let closed = match experiments.closed_retaining().await {
        Ok(closed) => closed,
        Err(e) => {
            tracing::warn!(error = %e, "prompt-log retention: closed experiment list unreadable");
            failures.push((0, e.to_string()));
            return (expired, failures);
        }
    };
    for exp in closed.into_iter().filter(|e| !retention_window_open(e, now)) {
        let id = exp.id;
        let redacted = match prompts.redact_experiment_content(id).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(experiment_id = id, error = %e, "experiment content redaction failed");
                failures.push((id, e.to_string()));
                // Keep the notes until the content goes with them: a half
                // redacted experiment is retried next tick either way.
                continue;
            }
        };
        let notes_cleared = match outcomes.clear_notes(id).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(experiment_id = id, error = %e, "experiment note clearing failed");
                failures.push((id, e.to_string()));
                0
            }
        };
        if redacted > 0 || notes_cleared > 0 {
            tracing::info!(
                experiment_id = id,
                redacted,
                notes_cleared,
                content_retention_days = exp.content_retention_days,
                "experiment content window elapsed"
            );
        }
        expired.push(ExpiredExperiment { experiment_id: id, redacted, notes_cleared });
    }
    (expired, failures)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::db::models::{NewExperiment, NewRunOutcome};
    use crate::db::prompt_store::CONTENT_NOT_STORED;
    use crate::db::sqlite::SqliteDb;

    const NOW: &str = "2026-09-10T12:00:00Z";

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(NOW).unwrap().with_timezone(&chrono::Utc)
    }

    async fn migrated(db: SqliteDb) -> SqliteDb {
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        sqlx::query("INSERT OR IGNORE INTO users (id, name, created_at) VALUES (1, 'test', '2025-01-01T00:00:00Z')")
            .execute(&db.pool)
            .await
            .unwrap();
        db
    }

    async fn memory_db() -> SqliteDb {
        migrated(SqliteDb::connect(":memory:").await.unwrap()).await
    }

    /// A prompt row stamped with `experiment_id` at `created_at`, carrying
    /// content and a latency sample.
    async fn insert_prompt(db: &SqliteDb, experiment_id: Option<i64>, created_at: &str) -> i64 {
        let r = sqlx::query(
            "INSERT INTO prompts (user_id, request_model, routed_model, provider, messages, response, \
             prompt_tokens, completion_tokens, cost_usd, latency_ms, tags, \
             attribution_correlation_id, attribution_tags, experiment_id, experiment_variant, created_at) \
             VALUES (1, 'planner', 'model-b', 'mock', '[{\"role\":\"user\",\"content\":\"plan the week\"}]', \
             'answer', 10, 20, 0.03, 250, '[]', 'run-1', '{}', ?, ?, ?)",
        )
        .bind(experiment_id)
        .bind(experiment_id.map(|_| "candidate"))
        .bind(created_at)
        .execute(&db.pool)
        .await
        .unwrap();
        r.last_insert_rowid()
    }

    async fn prompt(db: &SqliteDb, id: i64) -> Option<crate::db::models::Prompt> {
        PromptRepository::find_by_id(db, id).await.unwrap()
    }

    async fn seed(db: &SqliteDb, retain: bool, days: i64, closed_at: Option<&str>) -> i64 {
        let created = ExperimentRepository::create(
            db,
            NewExperiment {
                name: format!("exp-{}", rand_name()),
                variants: BTreeMap::new(),
                allowed_user_ids: vec![],
                feed_learning: false,
                expires_at: 0,
                retain_content: retain,
                content_retention_days: days,
            },
        )
        .await
        .unwrap();
        if let Some(at) = closed_at {
            assert!(ExperimentRepository::close(db, created.id, at).await.unwrap());
        }
        created.id
    }

    fn rand_name() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    async fn note(db: &SqliteDb, experiment_id: i64, run: &str) {
        OutcomeRepository::upsert(
            db,
            NewRunOutcome {
                user_id: 1,
                attribution_correlation_id: run.to_string(),
                outcome: "success".to_string(),
                score: Some(0.8),
                rating: Some(4),
                note: Some("kept the plan short".to_string()),
                experiment_id: Some(experiment_id),
                experiment_variant: Some("candidate".to_string()),
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn purge_keeps_open_retaining_rows_and_deletes_ordinary_ones() {
        let db = memory_db().await;
        let open = seed(&db, true, 0, None).await;
        let plain = seed(&db, false, 0, None).await;
        let old_plain = insert_prompt(&db, None, "2026-01-01T00:00:00Z").await;
        let old_unretained = insert_prompt(&db, Some(plain), "2026-01-01T00:00:00Z").await;
        let old_retained = insert_prompt(&db, Some(open), "2026-01-01T00:00:00Z").await;
        let fresh = insert_prompt(&db, None, "2026-09-10T00:00:00Z").await;

        let report = run_retention_tick(&db, &db, &db, 30, now()).await;
        assert_eq!(
            report.sweep,
            SweepOutcome::Purged { deleted: 2, protected: vec![open] }
        );
        assert!(report.expired.is_empty());
        assert!(report.failures.is_empty());
        assert!(prompt(&db, old_plain).await.is_none());
        assert!(prompt(&db, old_unretained).await.is_none());
        assert!(prompt(&db, old_retained).await.is_some());
        assert!(prompt(&db, fresh).await.is_some());
    }

    #[tokio::test]
    async fn elapsed_window_redacts_content_and_clears_notes_keeping_metadata() {
        let db = memory_db().await;
        // Closed two days ago with a one-day window: elapsed.
        let elapsed = seed(&db, true, 1, Some("2026-09-08T12:00:00Z")).await;
        // Closed two days ago with a window of never: untouched.
        let forever = seed(&db, true, 0, Some("2026-09-08T12:00:00Z")).await;
        // Closed an hour ago with a one-day window: still open.
        let within = seed(&db, true, 1, Some("2026-09-10T11:00:00Z")).await;
        let e_row = insert_prompt(&db, Some(elapsed), "2026-09-07T00:00:00Z").await;
        let f_row = insert_prompt(&db, Some(forever), "2026-09-07T00:00:00Z").await;
        let w_row = insert_prompt(&db, Some(within), "2026-09-10T10:00:00Z").await;
        note(&db, elapsed, "run-e").await;
        note(&db, forever, "run-f").await;
        note(&db, within, "run-w").await;

        let report = run_retention_tick(&db, &db, &db, 0, now()).await;
        assert_eq!(report.sweep, SweepOutcome::Disabled);
        assert_eq!(
            report.expired,
            vec![ExpiredExperiment { experiment_id: elapsed, redacted: 1, notes_cleared: 1 }]
        );
        assert!(report.failures.is_empty());

        let redacted = prompt(&db, e_row).await.unwrap();
        assert_eq!(redacted.messages, CONTENT_NOT_STORED);
        assert_eq!(redacted.response, None);
        assert_eq!(redacted.latency_ms, Some(250));
        assert_eq!(redacted.prompt_tokens, 10);
        assert_eq!(redacted.completion_tokens, 20);
        assert_eq!(redacted.experiment_id, Some(elapsed));
        assert_eq!(redacted.experiment_variant.as_deref(), Some("candidate"));
        assert_eq!(redacted.attribution_correlation_id.as_deref(), Some("run-1"));
        assert_eq!(redacted.created_at, "2026-09-07T00:00:00Z");
        assert!(OutcomeRepository::get(&db, 1, "run-e").await.unwrap().unwrap().note.is_none());

        for (row, run) in [(f_row, "run-f"), (w_row, "run-w")] {
            let kept = prompt(&db, row).await.unwrap();
            assert_eq!(kept.response.as_deref(), Some("answer"));
            assert!(kept.messages.contains("plan the week"));
            assert_eq!(
                OutcomeRepository::get(&db, 1, run).await.unwrap().unwrap().note.as_deref(),
                Some("kept the plan short")
            );
        }

        // A second tick finds nothing left to do for the elapsed experiment.
        let again = run_retention_tick(&db, &db, &db, 0, now()).await;
        assert_eq!(
            again.expired,
            vec![ExpiredExperiment { experiment_id: elapsed, redacted: 0, notes_cleared: 0 }]
        );
    }

    #[tokio::test]
    async fn separate_prompt_store_file_is_protected_by_the_same_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompts.db");
        let main = memory_db().await;
        let prompt_db = migrated(SqliteDb::connect(path.to_str().unwrap()).await.unwrap()).await;

        let open = seed(&main, true, 0, None).await;
        let elapsed = seed(&main, true, 1, Some("2026-09-01T00:00:00Z")).await;
        let kept = insert_prompt(&prompt_db, Some(open), "2026-01-01T00:00:00Z").await;
        let gone = insert_prompt(&prompt_db, None, "2026-01-01T00:00:00Z").await;
        let redacted = insert_prompt(&prompt_db, Some(elapsed), "2026-08-30T00:00:00Z").await;
        note(&main, elapsed, "run-e").await;

        let report = run_retention_tick(&main, &main, &prompt_db, 30, now()).await;
        assert_eq!(
            report.sweep,
            SweepOutcome::Purged { deleted: 1, protected: vec![open] }
        );
        assert_eq!(
            report.expired,
            vec![ExpiredExperiment { experiment_id: elapsed, redacted: 1, notes_cleared: 1 }]
        );
        assert!(prompt(&prompt_db, kept).await.unwrap().messages.contains("plan the week"));
        assert!(prompt(&prompt_db, gone).await.is_none());
        assert_eq!(prompt(&prompt_db, redacted).await.unwrap().messages, CONTENT_NOT_STORED);
        // The main database holds no prompt rows, and the tick did not
        // mistake it for the store.
        assert_eq!(PromptRepository::count(&main).await.unwrap(), 0);
        assert!(OutcomeRepository::get(&main, 1, "run-e").await.unwrap().unwrap().note.is_none());
    }

    #[tokio::test]
    async fn unreadable_experiment_list_skips_the_purge() {
        let main = memory_db().await;
        let prompt_db = memory_db().await;
        let open = seed(&main, true, 0, None).await;
        let kept = insert_prompt(&prompt_db, Some(open), "2026-01-01T00:00:00Z").await;
        let also_kept = insert_prompt(&prompt_db, None, "2026-01-01T00:00:00Z").await;

        // A closed pool fails every read, the way a locked or missing main
        // database would.
        main.pool.close().await;

        let report = run_retention_tick(&main, &main, &prompt_db, 30, now()).await;
        assert!(
            matches!(report.sweep, SweepOutcome::SkippedUnreadableExperiments(_)),
            "{:?}",
            report.sweep
        );
        assert!(report.expired.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].0, 0);
        assert!(prompt(&prompt_db, kept).await.is_some());
        assert!(prompt(&prompt_db, also_kept).await.is_some());
    }

    #[tokio::test]
    async fn a_failing_redaction_is_recorded_and_the_tick_goes_on() {
        let main = memory_db().await;
        let prompt_db = memory_db().await;
        let first = seed(&main, true, 1, Some("2026-09-01T00:00:00Z")).await;
        let second = seed(&main, true, 1, Some("2026-09-01T00:00:00Z")).await;
        note(&main, first, "run-1").await;
        note(&main, second, "run-2").await;

        // Content redaction cannot run against a store without the table;
        // every elapsed experiment is still attempted and reported.
        sqlx::query("DROP TABLE prompts").execute(&prompt_db.pool).await.unwrap();
        let report = run_retention_tick(&main, &main, &prompt_db, 0, now()).await;
        assert!(report.expired.is_empty());
        assert_eq!(report.failures.len(), 2);
        assert_eq!(report.failures[0].0, first);
        assert_eq!(report.failures[1].0, second);
        // Notes wait for the content to go.
        assert!(OutcomeRepository::get(&main, 1, "run-1").await.unwrap().unwrap().note.is_some());
        assert!(OutcomeRepository::get(&main, 1, "run-2").await.unwrap().unwrap().note.is_some());
    }
}
