use async_trait::async_trait;
use crate::db::models::{CostLedgerEntry, NewCostLedgerEntry, RunStamp};

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelSummaryRow {
    pub model: String,
    pub total_cost_usd: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub request_count: i64,
}

/// Cache-hit aggregates for one grouping key.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CacheUsageSummary {
    pub hits: i64,
    pub requests: i64,
    pub saved_usd: f64,
}

impl CacheUsageSummary {
    /// Fraction of requests served from cache, 0.0 when there were none.
    pub fn hit_rate(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.hits as f64 / self.requests as f64
        }
    }
}

/// Selects ledger rows by caller-supplied attribution (see `api::attribution`).
#[derive(Debug, Clone, PartialEq)]
pub enum AttributionFilter {
    /// Exact match on `attribution_correlation_id`.
    CorrelationId(String),
    /// Rows whose `attribution_tags` JSON object has `key` equal to `value`.
    Tag { key: String, value: String },
}

impl AttributionFilter {
    /// JSON path for the tag key, e.g. `$."engagement"`. Tag keys are validated
    /// on ingest to exclude quotes, backslashes and whitespace, so this cannot
    /// escape the path expression.
    pub fn tag_json_path(key: &str) -> String {
        format!("$.\"{}\"", key)
    }

    /// Human-readable label, used by the CLI and dashboard.
    pub fn label(&self) -> String {
        match self {
            AttributionFilter::CorrelationId(v) => format!("correlation_id={}", v),
            AttributionFilter::Tag { key, value } => format!("{}={}", key, value),
        }
    }
}

/// One "arm" of a side-by-side comparison: a slice of the ledger selected by
/// model, provider, or caller-supplied attribution. Every backend applies the
/// same window predicate (`created_at >= start AND created_at < end`) on top.
#[derive(Debug, Clone, PartialEq)]
pub enum ArmFilter {
    /// Exact match on the ledger `model` column.
    Model(String),
    /// Exact match on the ledger `provider` column.
    Provider(String),
    /// Attribution match (correlation id or tag), same semantics as
    /// [`AttributionFilter`].
    Attribution(AttributionFilter),
    /// Rows stamped with one variant of one experiment (spec §7a): exact
    /// match on `experiment_id` and `experiment_variant`.
    Variant { experiment_id: i64, variant: String },
}

impl ArmFilter {
    /// Human-readable label, used by the dashboard and CLI.
    pub fn label(&self) -> String {
        match self {
            ArmFilter::Model(m) => format!("model={}", m),
            ArmFilter::Provider(p) => format!("provider={}", p),
            ArmFilter::Attribution(f) => f.label(),
            ArmFilter::Variant { experiment_id, variant } => {
                format!("experiment={}:{}", experiment_id, variant)
            }
        }
    }
}

/// Spend *and* savings for one slice of attributed usage.
///
/// `cost_usd` is what was actually paid; `saved_usd` is what the response cache
/// avoided. Cache-hit rows always contribute to the second and never the first,
/// so the two never double-count.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct AttributionTotals {
    pub cost_usd: f64,
    pub saved_usd: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub requests: i64,
    pub cache_hits: i64,
}

impl AttributionTotals {
    /// Fraction of requests served from cache, 0.0 when there were none.
    pub fn hit_rate(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.cache_hits as f64 / self.requests as f64
        }
    }
}

/// One breakdown row: `key` is a model name or a calendar day depending on which
/// breakdown produced it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AttributionBreakdownRow {
    pub key: String,
    #[serde(flatten)]
    pub totals: AttributionTotals,
}

// ── Experiment results (spec §7a) ─────────────────────────────────────────────
//
// A run is every ledger row sharing `(user_id, attribution_correlation_id)`.
// Its variant is the variant of its earliest stamped row (by `created_at`,
// then id — the same rule as `CostRepository::run_stamp`); a run whose rows
// were stamped with more than one variant is *mixed*. Request-level figures
// are grouped by each row's own variant so they always sum to the experiment's
// totals; run-level attribution happens in Rust from [`ExperimentRunKey`].

/// Request-level aggregates for one variant: every ledger row stamped with it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ExperimentVariantTotals {
    pub variant: String,
    pub requests: i64,
    pub cost_usd: f64,
    pub saved_usd: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Rows whose token counts were estimated locally (`tokens_estimated`).
    pub estimated_rows: i64,
}

/// One model's share of one variant's stamped rows.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ExperimentVariantModelRow {
    pub variant: String,
    pub model: String,
    pub requests: i64,
    pub cost_usd: f64,
    pub saved_usd: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub estimated_rows: i64,
}

/// One run of an experiment, aggregated over its stamped ledger rows.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ExperimentRunRow {
    pub user_id: i64,
    pub correlation_id: String,
    /// Variant of the earliest stamped row.
    pub variant: String,
    /// Distinct variants seen on the run; more than one means mixed.
    pub variant_count: i64,
    pub requests: i64,
    pub cost_usd: f64,
    pub saved_usd: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub estimated_rows: i64,
    /// `created_at` of the earliest and latest stamped rows (RFC3339).
    pub first_at: String,
    pub last_at: String,
}

impl ExperimentRunRow {
    pub fn mixed(&self) -> bool {
        self.variant_count > 1
    }
}

/// The key, variant and run-level figures of one run — the light row the
/// results builder loads for *every* run of an experiment so run-level
/// attribution (run counts, turns, span, and the join to prompt rows,
/// failures and outcomes) happens in Rust, while [`ExperimentRunRow`] carries
/// the heavier per-run aggregates one page at a time.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ExperimentRunKey {
    pub user_id: i64,
    pub correlation_id: String,
    /// Variant of the earliest stamped row.
    pub variant: String,
    pub mixed: bool,
    /// Stamped ledger rows on the run.
    pub requests: i64,
    pub first_at: String,
    pub last_at: String,
}

/// Ledger rows with no experiment id that share a bound run's key: turns the
/// client sent without the header. Counted beside the run, never merged.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ExperimentUnboundRow {
    pub user_id: i64,
    pub correlation_id: String,
    pub requests: i64,
}

#[async_trait]
pub trait CostRepository: Send + Sync {
    async fn create(&self, entry: NewCostLedgerEntry) -> anyhow::Result<CostLedgerEntry>;
    /// Record a usage row that was served from the response cache.
    ///
    /// `entry.cost_usd` is interpreted as the cost that *would* have been paid:
    /// it is written to `saved_usd`, and `cost_usd` is forced to zero. A cache
    /// hit can therefore never be counted as spend, by construction.
    async fn create_cache_hit(&self, entry: NewCostLedgerEntry) -> anyhow::Result<CostLedgerEntry>;
    /// Cache hits / total requests / dollars saved since a timestamp,
    /// optionally narrowed to one model.
    async fn cache_summary_since(
        &self,
        filter_model: Option<&str>,
        since: &str,
    ) -> anyhow::Result<CacheUsageSummary>;
    /// Cache hits and total requests per calendar day, for the hit-rate-over-time
    /// chart. Returns (day, hits, requests) ascending by day.
    async fn cache_daily_series(
        &self,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<(String, i64, i64)>>;
    /// Cache aggregates grouped by model since a timestamp, most hits first.
    async fn cache_summary_by_model_since(
        &self,
        since: &str,
    ) -> anyhow::Result<Vec<(String, CacheUsageSummary)>>;
    /// Cache aggregates grouped by (user_id, model, project, api_key_id) —
    /// the same grouping key as [`CostRepository::cost_rows_grouped`], so the
    /// cost page can show a hit rate on every spend row.
    async fn cache_rows_grouped(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_api_key_id: Option<i64>,
        filter_model: Option<&str>,
        since: &str,
    ) -> anyhow::Result<Vec<(i64, String, Option<String>, Option<i64>, CacheUsageSummary)>>;
    async fn sum_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<f64>;
    async fn sum_tokens_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<i64>;
    async fn sum_for_key_since(&self, api_key_id: i64, since: &str) -> anyhow::Result<f64>;
    async fn sum_tokens_for_key_since(&self, api_key_id: i64, since: &str) -> anyhow::Result<i64>;
    async fn list_cost_entries_before(&self, cutoff: &str) -> anyhow::Result<Vec<crate::db::models::CostLedgerEntry>>;
    async fn delete_cost_entries_by_ids(&self, ids: &[i64]) -> anyhow::Result<()>;
    /// Sum spend for a user in [start, end) — both ISO 8601 UTC timestamps (inclusive start, exclusive end).
    async fn sum_for_user_between(&self, user_id: i64, start: &str, end: &str) -> anyhow::Result<f64>;
    /// Sum spend for a project since a timestamp (inclusive).
    /// Uses cost_ledger.project column (denormalized at write time).
    async fn sum_for_project_since(&self, project: &str, since: &str) -> anyhow::Result<f64>;
    /// Sum spend for a project in [start, end) (inclusive start, exclusive end).
    /// Uses cost_ledger.project column (denormalized at write time).
    async fn sum_for_project_between(&self, project: &str, start: &str, end: &str) -> anyhow::Result<f64>;
    /// Sum all spend across all users/projects since a timestamp (inclusive).
    async fn sum_global_since(&self, since: &str) -> anyhow::Result<f64>;
    /// Sum all spend across all users/projects in [start, end) (inclusive start, exclusive end).
    async fn sum_global_between(&self, start: &str, end: &str) -> anyhow::Result<f64>;
    /// Return (cost_usd, tokens_in, tokens_out, request_count) for a user since a timestamp.
    async fn user_cost_stats_since(&self, user_id: i64, since: &str) -> anyhow::Result<(f64, i64, i64, i64)>;
    /// Aggregate cost stats grouped by user_id with optional filters.
    /// Returns Vec of (user_id, cost_usd, tokens_in, tokens_out, request_count).
    /// `filter_user_ids`: None = all users; Some(&[]) = no users (empty result).
    /// `since`: ISO 8601 UTC; use "1970-01-01T00:00:00Z" for all-time.
    async fn cost_stats_grouped(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_api_key_id: Option<i64>,
        since: &str,
    ) -> anyhow::Result<Vec<(i64, f64, i64, i64, i64)>>;
    /// Distinct non-null project values present in the cost ledger, sorted.
    async fn distinct_projects_in_ledger(&self) -> anyhow::Result<Vec<String>>;
    /// Distinct non-null model values present in the cost ledger, sorted.
    async fn distinct_models_in_ledger(&self) -> anyhow::Result<Vec<String>>;
    /// Distinct non-null provider values present in the cost ledger, sorted.
    async fn distinct_providers_in_ledger(&self) -> anyhow::Result<Vec<String>>;
    /// Non-empty correlation ids ordered by most recent ledger row first,
    /// capped at `limit` — populates the "recent runs" picker.
    async fn distinct_recent_correlation_ids(&self, limit: i64) -> anyhow::Result<Vec<String>>;
    /// Experiment binding of a run: the experiment and variant of the
    /// earliest stamped ledger row (by `created_at`, then id) for this user
    /// and correlation id. `Some(RunStamp { None, None })` when the run has
    /// ledger rows but none is stamped; `None` when it has no rows at all.
    async fn run_stamp(&self, user_id: i64, correlation_id: &str) -> anyhow::Result<Option<RunStamp>>;
    /// Daily spend series: returns (date_str, cost_usd) pairs grouped by calendar day.
    /// `filter_user_ids`: None = all users; Some(&[]) = empty result.
    /// `start`/`end`: ISO 8601 UTC timestamps (inclusive start, exclusive end).
    async fn list_daily_spend(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_model: Option<&str>,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<(String, f64)>>;
    /// Aggregate cost stats grouped by model, with optional filters.
    async fn summarize_by_model(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_model: Option<&str>,
        since: &str,
    ) -> anyhow::Result<Vec<ModelSummaryRow>>;
    /// Per-row cost stats grouped by (user_id, model, project, api_key_id).
    /// Returns Vec of (user_id, model, project, api_key_id, cost_usd, tokens_in, tokens_out, request_count).
    /// Filters mirror cost_stats_grouped; adds an optional model filter.
    async fn cost_rows_grouped(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_api_key_id: Option<i64>,
        filter_model: Option<&str>,
        since: &str,
    ) -> anyhow::Result<Vec<(i64, String, Option<String>, Option<i64>, f64, i64, i64, i64)>>;

    // ── Attribution-filtered usage (issue #13) ────────────────────────────────
    //
    // These let a consuming app ask "what did *my* unit of work cost, and what
    // did the router save me on it" without keeping a parallel cost model.
    // `start`/`end` are ISO 8601 UTC (inclusive start, exclusive end).

    /// Spend, savings, tokens, requests and cache hits for one attribution value.
    async fn attribution_totals(
        &self,
        filter: &AttributionFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<AttributionTotals>;
    /// Same aggregates broken down by model, largest spend first.
    async fn attribution_by_model(
        &self,
        filter: &AttributionFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>>;
    /// Same aggregates broken down by calendar day, ascending.
    async fn attribution_by_day(
        &self,
        filter: &AttributionFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>>;
    /// Distinct tag keys present in the ledger, sorted — populates the pickers.
    async fn distinct_attribution_tag_keys(&self) -> anyhow::Result<Vec<String>>;
    /// Distinct values in the ledger for one tag key (or, when `key` is None,
    /// distinct correlation ids), sorted, capped at `limit`.
    async fn distinct_attribution_values(
        &self,
        key: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<String>>;
    /// Totals for one comparison arm within `[start, end)`.
    async fn arm_totals(
        &self,
        filter: &ArmFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<AttributionTotals>;
    /// Per-model breakdown for one comparison arm, highest spend first.
    async fn arm_by_model(
        &self,
        filter: &ArmFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>>;
    /// Per-calendar-day breakdown for one comparison arm, ascending by day.
    async fn arm_by_day(
        &self,
        filter: &ArmFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<AttributionBreakdownRow>>;

    // ── Experiment results (spec §7a) ──────────────────────────────────────

    /// Request-level totals per variant, every row stamped with the
    /// experiment, sorted by variant.
    async fn experiment_variant_totals(
        &self,
        experiment_id: i64,
    ) -> anyhow::Result<Vec<ExperimentVariantTotals>>;
    /// Per-variant, per-model breakdown of the stamped rows; sorted by
    /// variant, then highest spend first.
    async fn experiment_variant_models(
        &self,
        experiment_id: i64,
    ) -> anyhow::Result<Vec<ExperimentVariantModelRow>>;
    /// One page of runs, most recent activity first (ties broken by user and
    /// correlation id so paging is stable).
    async fn experiment_runs(
        &self,
        experiment_id: i64,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<ExperimentRunRow>>;
    /// Number of runs — distinct `(user_id, correlation_id)` — stamped with
    /// the experiment.
    async fn experiment_run_count(&self, experiment_id: i64) -> anyhow::Result<i64>;
    /// Key, variant, mixed flag, request count and span of every run,
    /// unpaginated, sorted by user and correlation id.
    async fn experiment_run_keys(
        &self,
        experiment_id: i64,
    ) -> anyhow::Result<Vec<ExperimentRunKey>>;
    /// Unstamped rows sharing a run's key, counted per run; runs with none
    /// are absent.
    async fn experiment_unbound_requests(
        &self,
        experiment_id: i64,
    ) -> anyhow::Result<Vec<ExperimentUnboundRow>>;
}
