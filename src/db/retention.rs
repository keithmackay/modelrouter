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
//! [`run_table_retention_tick`] is the tick's other job: `[retention]` for
//! the per-request tables the prompt-log policy does not cover (usage rows,
//! failures, hook metrics, the audit trail), each with its own opt-in day
//! count, followed by a space-reclaim pass. The usage-row cutoff is clamped
//! so it never cuts under a budget: not inside the current calendar month
//! (monthly budgets and reports read it), and not past the `window_start`
//! of any `total` budget rule; a `total` rule with no start holds the whole
//! ledger. A retention can therefore never make a budget under-count.
//!
//! The experiment list and the outcomes live in the main database; the
//! prompt rows live in the prompt store, which is the same database unless
//! `[storage] prompt_db_path` is set. The tick takes the two separately so
//! that split is honoured.

use crate::db::repositories::experiments::{retention_window_open, ExperimentRepository};
use crate::db::repositories::outcomes::OutcomeRepository;
use crate::db::repositories::prompts::PromptRepository;
use crate::config::schema::RetentionConfig;
use crate::db::models::BudgetRule;
use crate::db::repositories::audit::AuditRepository;
use crate::db::repositories::budgets::BudgetRepository;
use crate::db::repositories::costs::CostRepository;
use crate::db::repositories::failures::FailureRepository;
use crate::db::repositories::hooks::HookRepository;
use crate::db::repositories::maintenance::{MaintenanceRepository, ReclaimOutcome};
use crate::router::policy::window_start_at;

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

/// The repositories the table tick needs: the budget rules (to hold the
/// usage-row cutoff above them), the four tables, and the engine's reclaim.
pub trait RetentionStore:
    BudgetRepository + CostRepository + FailureRepository + HookRepository + AuditRepository + MaintenanceRepository
{
}

impl<T: ?Sized> RetentionStore for T where
    T: BudgetRepository + CostRepository + FailureRepository + HookRepository + AuditRepository + MaintenanceRepository
{
}

/// The three tables with a plain `created_at`-style cutoff (usage rows have
/// their own path, for the budget clamp).
#[derive(Debug, Clone, Copy)]
enum PlainTable {
    RequestFailures,
    HookMetrics,
    AuditLog,
}

impl PlainTable {
    fn name(self) -> &'static str {
        match self {
            PlainTable::RequestFailures => "request_failures",
            PlainTable::HookMetrics => "hook_metrics",
            PlainTable::AuditLog => "audit_log",
        }
    }
}

/// What held the usage-row cutoff earlier than the configured retention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerFloor {
    /// The start of the current calendar month: monthly budgets and reports
    /// read from there whenever they are created.
    CalendarMonth,
    /// A `total` budget rule reads from its `window_start`.
    BudgetRule { rule_id: i64, window_start: String },
}

/// Why no usage row was deleted this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerHold {
    /// A `total` budget rule with no `window_start` reads the ledger from
    /// the epoch. Delete the rule or give it a start to let retention run.
    Unbounded { rule_id: i64 },
    /// A `total` rule's `window_start` is not a date the tick can read, so
    /// it cannot tell what the rule reads and keeps everything.
    Unparseable { rule_id: i64, window_start: String },
    /// The budget rules could not be read, so the floor is unknown.
    RulesUnreadable(String),
}

/// How one table's purge ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TablePurge {
    /// Retention `0`: the table is kept forever.
    Disabled,
    /// Rows before `cutoff` were deleted.
    Purged { deleted: u64, cutoff: String },
    /// Usage rows only: a budget rule holds the whole ledger, nothing deleted.
    Held(LedgerHold),
    /// The DELETE failed; retried next tick.
    Failed(String),
}

/// Everything one table tick did, for the log line and for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRetentionReport {
    pub cost_ledger: TablePurge,
    /// Set when the usage-row cutoff was moved earlier than
    /// `cost_ledger_days` asked for, and what moved it.
    pub ledger_clamped: Option<LedgerFloor>,
    pub request_failures: TablePurge,
    pub hook_metrics: TablePurge,
    pub audit_log: TablePurge,
    /// The space-reclaim pass; `Err` carries the engine's message.
    pub reclaim: Result<ReclaimOutcome, String>,
}

impl TableRetentionReport {
    /// Rows deleted across the four tables.
    pub fn deleted(&self) -> u64 {
        [&self.cost_ledger, &self.request_failures, &self.hook_metrics, &self.audit_log]
            .into_iter()
            .map(|t| match t {
                TablePurge::Purged { deleted, .. } => *deleted,
                _ => 0,
            })
            .sum()
    }
}

/// Run one `[retention]` tick at `now` against `store`, then reclaim space.
/// Never panics and never returns an error: each table reports its own
/// outcome, so one failing table does not stop the others.
pub async fn run_table_retention_tick<S: RetentionStore + ?Sized>(
    store: &S,
    policy: &RetentionConfig,
    now: chrono::DateTime<chrono::Utc>,
) -> TableRetentionReport {
    let (cost_ledger, ledger_clamped) = purge_cost_ledger(store, policy.cost_ledger_days, now).await;
    let request_failures =
        purge_table(store, PlainTable::RequestFailures, policy.request_failures_days, now).await;
    let hook_metrics = purge_table(store, PlainTable::HookMetrics, policy.hook_metrics_days, now).await;
    let audit_log = purge_table(store, PlainTable::AuditLog, policy.audit_log_days, now).await;

    let reclaim = match store.reclaim_space().await {
        Ok(outcome) => {
            match &outcome {
                ReclaimOutcome::IncrementalVacuum { freed_pages } if *freed_pages > 0 => {
                    tracing::info!(freed_pages, "retention: free pages returned to the file system");
                }
                ReclaimOutcome::NotIncremental { free_pages } if *free_pages > 0 => {
                    tracing::info!(
                        free_pages,
                        "retention: the database file is not in incremental auto-vacuum mode, so it keeps \
                         its free pages for reuse and does not shrink; `modelrouter db vacuum` converts it"
                    );
                }
                _ => {}
            }
            Ok(outcome)
        }
        Err(e) => {
            tracing::warn!(error = %e, "retention: space reclaim failed");
            Err(e.to_string())
        }
    };

    TableRetentionReport { cost_ledger, ledger_clamped, request_failures, hook_metrics, audit_log, reclaim }
}

/// `now - days`, or `None` for a span the clock cannot represent (treated
/// as keep forever, like the prompt sweep).
fn cutoff_for(days: u64, now: chrono::DateTime<chrono::Utc>) -> Option<chrono::DateTime<chrono::Utc>> {
    i64::try_from(days)
        .ok()
        .and_then(chrono::Duration::try_days)
        .and_then(|d| now.checked_sub_signed(d))
}

async fn purge_table<S: RetentionStore + ?Sized>(
    store: &S,
    table: PlainTable,
    days: u64,
    now: chrono::DateTime<chrono::Utc>,
) -> TablePurge {
    if days == 0 {
        return TablePurge::Disabled;
    }
    let Some(cutoff) = cutoff_for(days, now) else {
        return TablePurge::Disabled;
    };
    let cutoff = cutoff.to_rfc3339();
    let purged = match table {
        PlainTable::RequestFailures => store.purge_failures_older_than(&cutoff).await,
        PlainTable::HookMetrics => store.purge_metrics_older_than(&cutoff).await,
        PlainTable::AuditLog => store.purge_audit_older_than(&cutoff).await,
    };
    let table = table.name();
    match purged {
        Ok(deleted) => {
            if deleted > 0 {
                tracing::info!(table, deleted, retention_days = days, %cutoff, "retention purge");
            }
            TablePurge::Purged { deleted, cutoff }
        }
        Err(e) => {
            tracing::warn!(table, error = %e, "retention purge failed");
            TablePurge::Failed(e.to_string())
        }
    }
}

/// The earliest moment any budget still reads the usage rows from: the
/// start of the current calendar month, lowered to the `window_start` of
/// every `total` rule. `Err` names the rule that holds the whole ledger.
fn ledger_floor(
    rules: &[BudgetRule],
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(chrono::DateTime<chrono::Utc>, LedgerFloor), LedgerHold> {
    let month = chrono::DateTime::parse_from_rfc3339(&window_start_at("monthly", now))
        .map(|d| d.with_timezone(&chrono::Utc))
        .map_err(|e| LedgerHold::RulesUnreadable(format!("month start unreadable: {e}")))?;
    let mut floor = (month, LedgerFloor::CalendarMonth);
    for rule in rules {
        if rule.window != "total" {
            // daily, weekly and monthly windows all start inside the month.
            continue;
        }
        let Some(start) = rule.window_start.as_deref() else {
            return Err(LedgerHold::Unbounded { rule_id: rule.id });
        };
        let parsed = chrono::DateTime::parse_from_rfc3339(start)
            .map(|d| d.with_timezone(&chrono::Utc))
            .or_else(|_| {
                chrono::NaiveDate::parse_from_str(start, "%Y-%m-%d")
                    .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc())
            });
        let Ok(start_at) = parsed else {
            return Err(LedgerHold::Unparseable { rule_id: rule.id, window_start: start.to_string() });
        };
        if start_at < floor.0 {
            floor = (start_at, LedgerFloor::BudgetRule { rule_id: rule.id, window_start: start.to_string() });
        }
    }
    Ok(floor)
}

async fn purge_cost_ledger<S: RetentionStore + ?Sized>(
    store: &S,
    days: u64,
    now: chrono::DateTime<chrono::Utc>,
) -> (TablePurge, Option<LedgerFloor>) {
    if days == 0 {
        return (TablePurge::Disabled, None);
    }
    let Some(wanted) = cutoff_for(days, now) else {
        return (TablePurge::Disabled, None);
    };
    let rules = match store.list_all().await {
        Ok(rules) => rules,
        Err(e) => {
            tracing::warn!(error = %e, "retention: budget rules unreadable, keeping every usage row this tick");
            return (TablePurge::Held(LedgerHold::RulesUnreadable(e.to_string())), None);
        }
    };
    let (floor, floor_source) = match ledger_floor(&rules, now) {
        Ok(floor) => floor,
        Err(hold) => {
            tracing::info!(?hold, "retention: a budget rule holds the usage rows, nothing deleted");
            return (TablePurge::Held(hold), None);
        }
    };
    let (cutoff, clamped) = if floor < wanted { (floor, Some(floor_source)) } else { (wanted, None) };
    let cutoff_str = cutoff.to_rfc3339();
    match store.purge_ledger_older_than(&cutoff_str).await {
        Ok(deleted) => {
            if deleted > 0 {
                tracing::info!(
                    table = "cost_ledger",
                    deleted,
                    retention_days = days,
                    cutoff = %cutoff_str,
                    clamped_by = ?clamped,
                    "retention purge"
                );
            }
            (TablePurge::Purged { deleted, cutoff: cutoff_str }, clamped)
        }
        Err(e) => {
            tracing::warn!(table = "cost_ledger", error = %e, "retention purge failed");
            (TablePurge::Failed(e.to_string()), clamped)
        }
    }
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
    async fn the_purge_detaches_usage_rows_from_the_prompts_it_deletes() {
        let db = memory_db().await;
        let open = seed(&db, true, 0, None).await;
        let old = insert_prompt(&db, None, "2026-01-01T00:00:00Z").await;
        let old_protected = insert_prompt(&db, Some(open), "2026-01-01T00:00:00Z").await;
        let fresh = insert_prompt(&db, None, "2026-09-10T00:00:00Z").await;
        for prompt_id in [old, old_protected, fresh] {
            sqlx::query(
                "INSERT INTO cost_ledger (user_id, prompt_id, model, provider, cost_usd, created_at) \
                 VALUES (1, ?, 'model-b', 'mock', 0.03, (SELECT created_at FROM prompts WHERE id = ?))",
            )
            .bind(prompt_id)
            .bind(prompt_id)
            .execute(&db.pool)
            .await
            .unwrap();
        }

        // Foreign keys are enforced on this pool, so without the detach the
        // DELETE fails on the first referenced prompt.
        let (fk,): (i64,) = sqlx::query_as("PRAGMA foreign_keys").fetch_one(&db.pool).await.unwrap();
        assert_eq!(fk, 1);

        let report = run_retention_tick(&db, &db, &db, 30, now()).await;
        assert_eq!(report.sweep, SweepOutcome::Purged { deleted: 1, protected: vec![open] });
        assert!(prompt(&db, old).await.is_none());
        assert!(prompt(&db, old_protected).await.is_some());
        assert!(prompt(&db, fresh).await.is_some());

        // Every usage row survives; only the deleted prompt's link is gone.
        let links: Vec<Option<i64>> = sqlx::query_scalar("SELECT prompt_id FROM cost_ledger ORDER BY id")
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert_eq!(links, vec![None, Some(old_protected), Some(fresh)]);
        assert_eq!(CostRepository::sum_for_user_since(&db, 1, "1970-01-01T00:00:00Z").await.unwrap(), 0.09);
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

#[cfg(test)]
mod table_tests {
    use super::*;
    use crate::db::models::NewBudgetRule;
    use crate::db::sqlite::SqliteDb;

    const NOW: &str = "2026-09-10T12:00:00Z";
    const MONTH_START: &str = "2026-09-01T00:00:00+00:00";

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(NOW).unwrap().with_timezone(&chrono::Utc)
    }

    async fn memory_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        sqlx::query("INSERT OR IGNORE INTO users (id, name, created_at) VALUES (1, 'test', '2025-01-01T00:00:00Z')")
            .execute(&db.pool)
            .await
            .unwrap();
        db
    }

    async fn ledger_row(db: &SqliteDb, created_at: &str) {
        sqlx::query(
            "INSERT INTO cost_ledger (user_id, prompt_id, model, provider, cost_usd, created_at) \
             VALUES (1, NULL, 'm', 'p', 1.0, ?)",
        )
        .bind(created_at)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    async fn failure_row(db: &SqliteDb, created_at: &str) {
        sqlx::query(
            "INSERT INTO request_failures (endpoint, request_model, stage, error_message, attempts, attribution_tags, created_at) \
             VALUES ('/v1/chat/completions', 'm', 'provider', 'boom', 1, '{}', ?)",
        )
        .bind(created_at)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    async fn hook_row(db: &SqliteDb, invoked_at: &str) {
        sqlx::query("INSERT INTO hook_metrics (hook_name, invoked_at, duration_ms, success) VALUES ('h', ?, 3, 1)")
            .bind(invoked_at)
            .execute(&db.pool)
            .await
            .unwrap();
    }

    async fn audit_row(db: &SqliteDb, created_at: &str) {
        sqlx::query("INSERT INTO audit_log (actor_name, action, created_at) VALUES ('op', 'test', ?)")
            .bind(created_at)
            .execute(&db.pool)
            .await
            .unwrap();
    }

    async fn count(db: &SqliteDb, table: &str) -> i64 {
        let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}"))).fetch_one(&db.pool).await.unwrap();
        n
    }

    async fn ledger_dates(db: &SqliteDb) -> Vec<String> {
        sqlx::query_scalar("SELECT created_at FROM cost_ledger ORDER BY created_at")
            .fetch_all(&db.pool)
            .await
            .unwrap()
    }

    async fn total_rule(db: &SqliteDb, window_start: Option<&str>) -> i64 {
        BudgetRepository::create(
            db,
            NewBudgetRule {
                user_id: None,
                group_name: None,
                api_key_id: None,
                tag: None,
                window: "total".to_string(),
                limit_usd: Some(500.0),
                limit_tokens: None,
                model_allow: vec![],
                model_deny: vec![],
                rate_rpm: None,
                max_concurrent: None,
                project: None,
                window_start: window_start.map(str::to_string),
                window_end: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    fn policy(cost: u64, failures: u64, hooks: u64, audit: u64) -> RetentionConfig {
        RetentionConfig {
            cost_ledger_days: cost,
            request_failures_days: failures,
            hook_metrics_days: hooks,
            audit_log_days: audit,
        }
    }

    fn purged(report: &TablePurge) -> (u64, &str) {
        match report {
            TablePurge::Purged { deleted, cutoff } => (*deleted, cutoff.as_str()),
            other => panic!("expected a purge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_default_policy_keeps_every_table() {
        let db = memory_db().await;
        for at in ["2020-01-01T00:00:00Z", "2026-09-10T11:00:00Z"] {
            ledger_row(&db, at).await;
            failure_row(&db, at).await;
            hook_row(&db, at).await;
            audit_row(&db, at).await;
        }
        let report = run_table_retention_tick(&db, &RetentionConfig::default(), now()).await;
        assert_eq!(report.cost_ledger, TablePurge::Disabled);
        assert_eq!(report.request_failures, TablePurge::Disabled);
        assert_eq!(report.hook_metrics, TablePurge::Disabled);
        assert_eq!(report.audit_log, TablePurge::Disabled);
        assert_eq!(report.ledger_clamped, None);
        assert_eq!(report.deleted(), 0);
        assert!(report.reclaim.is_ok(), "{:?}", report.reclaim);
        for table in ["cost_ledger", "request_failures", "hook_metrics", "audit_log"] {
            assert_eq!(count(&db, table).await, 2, "{table}");
        }
    }

    #[tokio::test]
    async fn each_table_keeps_its_own_window() {
        let db = memory_db().await;
        // One row just inside and one just outside each table's window.
        ledger_row(&db, "2026-06-13T00:00:00Z").await; // 89 days: kept at 90
        ledger_row(&db, "2026-06-11T00:00:00Z").await; // 91 days: gone
        failure_row(&db, "2026-08-12T00:00:00Z").await; // 29 days: kept at 30
        failure_row(&db, "2026-08-10T00:00:00Z").await; // 31 days: gone
        hook_row(&db, "2026-09-04T00:00:00Z").await; // 6 days: kept at 7
        hook_row(&db, "2026-09-02T00:00:00Z").await; // 8 days: gone
        audit_row(&db, "2025-09-11T00:00:00Z").await; // 364 days: kept at 365
        audit_row(&db, "2025-09-09T00:00:00Z").await; // 366 days: gone

        let report = run_table_retention_tick(&db, &policy(90, 30, 7, 365), now()).await;
        assert_eq!(purged(&report.cost_ledger), (1, "2026-06-12T12:00:00+00:00"));
        assert_eq!(purged(&report.request_failures), (1, "2026-08-11T12:00:00+00:00"));
        assert_eq!(purged(&report.hook_metrics), (1, "2026-09-03T12:00:00+00:00"));
        assert_eq!(purged(&report.audit_log), (1, "2025-09-10T12:00:00+00:00"));
        assert_eq!(report.ledger_clamped, None, "90 days reaches past the month start");
        assert_eq!(report.deleted(), 4);
        for table in ["cost_ledger", "request_failures", "hook_metrics", "audit_log"] {
            assert_eq!(count(&db, table).await, 1, "{table}");
        }
        // Idempotent: the next tick finds nothing older.
        let again = run_table_retention_tick(&db, &policy(90, 30, 7, 365), now()).await;
        assert_eq!(again.deleted(), 0);
    }

    #[tokio::test]
    async fn the_usage_cutoff_never_enters_the_current_month() {
        let db = memory_db().await;
        ledger_row(&db, "2026-08-31T23:00:00Z").await; // last month: gone
        ledger_row(&db, "2026-09-01T06:00:00Z").await; // this month, 9 days old: kept at 1 day
        ledger_row(&db, "2026-09-10T11:00:00Z").await;

        let report = run_table_retention_tick(&db, &policy(1, 0, 0, 0), now()).await;
        assert_eq!(purged(&report.cost_ledger), (1, MONTH_START));
        assert_eq!(report.ledger_clamped, Some(LedgerFloor::CalendarMonth));
        assert_eq!(ledger_dates(&db).await, vec!["2026-09-01T06:00:00Z", "2026-09-10T11:00:00Z"]);
    }

    #[tokio::test]
    async fn a_total_rule_holds_the_usage_rows_back_to_its_start() {
        let db = memory_db().await;
        let id = total_rule(&db, Some("2026-03-01T00:00:00Z")).await;
        ledger_row(&db, "2026-02-28T00:00:00Z").await; // before the rule's start: gone
        ledger_row(&db, "2026-03-15T00:00:00Z").await; // the rule reads it: kept at 30 days
        ledger_row(&db, "2026-09-09T00:00:00Z").await;

        let report = run_table_retention_tick(&db, &policy(30, 0, 0, 0), now()).await;
        assert_eq!(purged(&report.cost_ledger), (1, "2026-03-01T00:00:00+00:00"));
        assert_eq!(
            report.ledger_clamped,
            Some(LedgerFloor::BudgetRule { rule_id: id, window_start: "2026-03-01T00:00:00Z".to_string() })
        );
        assert_eq!(ledger_dates(&db).await, vec!["2026-03-15T00:00:00Z", "2026-09-09T00:00:00Z"]);
    }

    #[tokio::test]
    async fn daily_and_weekly_rules_hold_nothing_past_the_month() {
        let db = memory_db().await;
        for window in ["daily", "weekly", "monthly"] {
            BudgetRepository::create(
                &db,
                NewBudgetRule {
                    user_id: Some(1),
                    group_name: None,
                    api_key_id: None,
                    tag: None,
                    window: window.to_string(),
                    limit_usd: Some(5.0),
                    limit_tokens: None,
                    model_allow: vec![],
                    model_deny: vec![],
                    rate_rpm: None,
                    max_concurrent: None,
                    project: None,
                    window_start: None,
                    window_end: None,
                },
            )
            .await
            .unwrap();
        }
        ledger_row(&db, "2026-08-31T23:00:00Z").await;
        let report = run_table_retention_tick(&db, &policy(1, 0, 0, 0), now()).await;
        assert_eq!(purged(&report.cost_ledger), (1, MONTH_START));
        assert_eq!(report.ledger_clamped, Some(LedgerFloor::CalendarMonth));
    }

    #[tokio::test]
    async fn a_total_rule_without_a_start_holds_the_whole_ledger() {
        let db = memory_db().await;
        let id = total_rule(&db, None).await;
        ledger_row(&db, "2020-01-01T00:00:00Z").await;
        failure_row(&db, "2020-01-01T00:00:00Z").await;

        let report = run_table_retention_tick(&db, &policy(30, 30, 0, 0), now()).await;
        assert_eq!(report.cost_ledger, TablePurge::Held(LedgerHold::Unbounded { rule_id: id }));
        assert_eq!(report.ledger_clamped, None);
        assert_eq!(count(&db, "cost_ledger").await, 1, "nothing deleted under an unbounded rule");
        // The other tables are not held by a budget.
        assert_eq!(purged(&report.request_failures).0, 1);
        assert_eq!(count(&db, "request_failures").await, 0);
    }

    #[tokio::test]
    async fn an_unreadable_window_start_holds_the_ledger_and_names_the_rule() {
        let db = memory_db().await;
        let id = total_rule(&db, Some("last tuesday")).await;
        ledger_row(&db, "2020-01-01T00:00:00Z").await;
        let report = run_table_retention_tick(&db, &policy(30, 0, 0, 0), now()).await;
        assert_eq!(
            report.cost_ledger,
            TablePurge::Held(LedgerHold::Unparseable { rule_id: id, window_start: "last tuesday".to_string() })
        );
        assert_eq!(count(&db, "cost_ledger").await, 1);

        // A plain date is a start the tick can read.
        let dated = total_rule(&db, Some("2019-06-01")).await;
        BudgetRepository::delete(&db, id).await.unwrap();
        let report = run_table_retention_tick(&db, &policy(30, 0, 0, 0), now()).await;
        assert_eq!(purged(&report.cost_ledger), (0, "2019-06-01T00:00:00+00:00"));
        assert_eq!(
            report.ledger_clamped,
            Some(LedgerFloor::BudgetRule { rule_id: dated, window_start: "2019-06-01".to_string() })
        );
    }

    #[tokio::test]
    async fn unreadable_budget_rules_keep_the_usage_rows_and_purge_the_rest() {
        let db = memory_db().await;
        ledger_row(&db, "2020-01-01T00:00:00Z").await;
        hook_row(&db, "2020-01-01T00:00:00Z").await;
        audit_row(&db, "2020-01-01T00:00:00Z").await;
        sqlx::query("DROP TABLE budget_rules").execute(&db.pool).await.unwrap();

        let report = run_table_retention_tick(&db, &policy(30, 0, 7, 7), now()).await;
        assert!(matches!(report.cost_ledger, TablePurge::Held(LedgerHold::RulesUnreadable(_))), "{:?}", report.cost_ledger);
        assert_eq!(count(&db, "cost_ledger").await, 1);
        assert_eq!(purged(&report.hook_metrics).0, 1);
        assert_eq!(purged(&report.audit_log).0, 1);
    }

    #[tokio::test]
    async fn a_failing_table_is_reported_and_the_others_still_run() {
        let db = memory_db().await;
        failure_row(&db, "2020-01-01T00:00:00Z").await;
        hook_row(&db, "2020-01-01T00:00:00Z").await;
        sqlx::query("DROP TABLE hook_metrics").execute(&db.pool).await.unwrap();

        let report = run_table_retention_tick(&db, &policy(0, 7, 7, 7), now()).await;
        assert!(matches!(report.hook_metrics, TablePurge::Failed(ref e) if e.contains("hook_metrics")), "{:?}", report.hook_metrics);
        assert_eq!(purged(&report.request_failures).0, 1);
        assert_eq!(purged(&report.audit_log).0, 0);
        assert!(report.reclaim.is_ok());
    }

    #[tokio::test]
    async fn an_absurd_retention_keeps_the_table() {
        let db = memory_db().await;
        ledger_row(&db, "2020-01-01T00:00:00Z").await;
        let report = run_table_retention_tick(&db, &policy(u64::MAX, u64::MAX, 0, 0), now()).await;
        assert_eq!(report.cost_ledger, TablePurge::Disabled);
        assert_eq!(report.request_failures, TablePurge::Disabled);
        assert_eq!(count(&db, "cost_ledger").await, 1);
    }

    #[tokio::test]
    async fn a_file_database_reclaims_what_the_purges_free() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("router.db");
        let db = SqliteDb::connect(path.to_str().unwrap()).await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        let payload = "x".repeat(4000);
        for i in 0..300 {
            sqlx::query("INSERT INTO audit_log (actor_name, action, target, created_at) VALUES ('op', 'test', ?, ?)")
                .bind(&payload)
                .bind(format!("2020-01-01T00:{:02}:{:02}Z", i / 60, i % 60))
                .execute(&db.pool)
                .await
                .unwrap();
        }
        let (pages_before, _) = db.page_count_and_size().await.unwrap();
        let report = run_table_retention_tick(&db, &policy(0, 0, 0, 30), now()).await;
        assert_eq!(purged(&report.audit_log).0, 300);
        assert!(
            matches!(report.reclaim, Ok(ReclaimOutcome::IncrementalVacuum { freed_pages }) if freed_pages > 0),
            "{:?}",
            report.reclaim
        );
        let (pages_after, _) = db.page_count_and_size().await.unwrap();
        assert!(pages_after < pages_before, "{pages_after} < {pages_before}");
    }
}
