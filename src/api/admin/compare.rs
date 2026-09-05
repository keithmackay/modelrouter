//! Side-by-side comparison of two experiment arms (spec §7b).
//!
//! An arm is a slice of recorded traffic — one model, one provider, one tag
//! value, one correlation id, or one variant of a controlled experiment (spec
//! §7a). For the first four the router never assigns arms; a client forms them
//! by choosing a model per arm and tagging each request. A variant arm is the
//! rows the router stamped while the request was bound to that variant. Either
//! way this module only partitions what the ledger, the prompt log and the
//! failure log recorded.
//!
//! One builder serves three consumers: the JSON endpoint here, the dashboard
//! panels, and `modelrouter report compare`. The dashboard and CLI must show
//! the same numbers, so neither computes anything of its own.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::Html;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::app::{AppState, DatabaseProvider};
use crate::api::error::ApiError;
use crate::db::models::{Experiment, ExperimentStatus};
use crate::db::repositories::costs::{
    ArmFilter, AttributionBreakdownRow, AttributionFilter, CostRepository,
};
use crate::db::repositories::experiments::{ExperimentRepository, ExperimentStatusFilter};
use crate::db::repositories::failures::FailureRepository;
use crate::db::repositories::prompts::{LatencySummary, PromptRepository};
use crate::router::cost::CostCalculator;
use crate::router::experiments::is_valid_label;

use super::attribution::{window_range, FACET_LIMIT};
use super::auth::AdminSession;
use super::dashboard::{DashboardError, DashboardSession};

/// Statements every comparison carries, in order. The page and the CLI print
/// them verbatim; tests pin their presence, not their wording.
pub const CAVEAT_QUALITY: &str = "This comparison has no quality column. A difference in cost or \
    latency is not evidence of a difference in answer quality.";
/// Replaces `CAVEAT_QUALITY` for the variant dimension: an experiment's runs
/// can carry reported outcomes, but they are read on its results page, not here.
pub const CAVEAT_QUALITY_VARIANT: &str = "This comparison has no quality column. A difference \
    in cost or latency is not evidence of a difference in answer quality; the outcomes reported \
    for this experiment's runs are on its results page under /admin/experiments.";
pub const CAVEAT_STREAMING: &str = "Streamed responses record estimated or zero tokens and, on the \
    messages API, a placeholder latency; they are indistinguishable from measured rows here. \
    Send experiment traffic with stream: false.";
pub const TTFT_NOTE: &str =
    "Time to first token is not recorded by the router today, so it cannot be compared.";

pub const DIMENSIONS: [&str; 5] = ["model", "provider", "tag", "run", "variant"];
pub const WINDOWS: [&str; 4] = ["all", "daily", "weekly", "monthly"];

// ── Query ─────────────────────────────────────────────────────────────────────

/// Query shared by the REST endpoint, the dashboard panels and the CLI.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompareQuery {
    #[serde(default)]
    pub dimension: String,
    /// Tag key when `dimension = tag`; experiment id when `dimension =
    /// variant`; ignored otherwise.
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub a: String,
    #[serde(default)]
    pub b: String,
    #[serde(default = "super::attribution::default_window")]
    pub window: String,
}

/// A query that has passed validation: two arm filters and a window.
#[derive(Debug, Clone)]
pub struct ValidatedQuery {
    pub dimension: String,
    pub key: Option<String>,
    /// The parsed `key` when `dimension = variant`.
    pub experiment_id: Option<i64>,
    pub a: String,
    pub b: String,
    pub arm_a: ArmFilter,
    pub arm_b: ArmFilter,
    pub window: String,
    pub start: String,
    pub end: String,
}

impl CompareQuery {
    /// Validate every field before anything reaches SQL. Errors name the field.
    pub fn validate(&self) -> Result<ValidatedQuery, CompareError> {
        let dimension = self.dimension.trim();
        if !DIMENSIONS.contains(&dimension) {
            return Err(CompareError::Invalid(format!(
                "dimension must be one of {}",
                DIMENSIONS.join(", ")
            )));
        }
        let a = self.a.trim();
        let b = self.b.trim();
        if a.is_empty() {
            return Err(CompareError::Invalid("a is required".to_string()));
        }
        if b.is_empty() {
            return Err(CompareError::Invalid("b is required".to_string()));
        }
        if a == b {
            return Err(CompareError::Invalid("a and b must differ".to_string()));
        }
        let window = self.window.trim();
        if !WINDOWS.contains(&window) {
            return Err(CompareError::Invalid(format!(
                "window must be one of {}",
                WINDOWS.join(", ")
            )));
        }

        let mut experiment_id = None;
        let key = match dimension {
            "tag" => {
                let key = self.key.trim();
                if key.is_empty() {
                    return Err(CompareError::Invalid(
                        "key is required when dimension=tag".to_string(),
                    ));
                }
                if !crate::api::attribution::is_safe_tag_key(key) {
                    return Err(CompareError::Invalid(
                        "attribution tag key must contain only letters, digits, '_', '-', '.' or ':'"
                            .to_string(),
                    ));
                }
                Some(key.to_string())
            }
            "variant" => {
                // The id and the labels are checked here, without the
                // database, so the CLI fails fast on a typo; whether the
                // experiment exists and declares the labels is checked by
                // `build_comparison`, which is the first place with a handle.
                let key = self.key.trim();
                let id = match key.parse::<i64>() {
                    Ok(id) if id > 0 => id,
                    _ => {
                        return Err(CompareError::Invalid(
                            "key must be an experiment id (a positive integer) when dimension=variant"
                                .to_string(),
                        ))
                    }
                };
                for (field, label) in [("a", a), ("b", b)] {
                    if !is_valid_label(label) {
                        return Err(CompareError::Invalid(format!(
                            "{field} must be a variant label: letters, digits, '_', '.' or '-', \
                             at most {} characters",
                            crate::router::experiments::MAX_LABEL_LEN
                        )));
                    }
                }
                experiment_id = Some(id);
                Some(id.to_string())
            }
            _ => None,
        };

        let arm = |value: &str| match dimension {
            "model" => ArmFilter::Model(value.to_string()),
            "provider" => ArmFilter::Provider(value.to_string()),
            "tag" => ArmFilter::Attribution(AttributionFilter::Tag {
                key: key.clone().unwrap_or_default(),
                value: value.to_string(),
            }),
            "variant" => ArmFilter::Variant {
                experiment_id: experiment_id.unwrap_or_default(),
                variant: value.to_string(),
            },
            _ => ArmFilter::Attribution(AttributionFilter::CorrelationId(value.to_string())),
        };
        let (arm_a, arm_b) = (arm(a), arm(b));
        let (start, end) = window_range(window);
        Ok(ValidatedQuery {
            dimension: dimension.to_string(),
            key,
            experiment_id,
            a: a.to_string(),
            b: b.to_string(),
            arm_a,
            arm_b,
            window: window.to_string(),
            start,
            end,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CompareError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Db(#[from] anyhow::Error),
}

impl From<CompareError> for ApiError {
    fn from(e: CompareError) -> Self {
        match e {
            CompareError::Invalid(msg) => ApiError::InvalidRequest(msg),
            CompareError::Db(err) => {
                tracing::error!(error = %err, "comparison query failed");
                ApiError::Internal
            }
        }
    }
}

impl From<CompareError> for super::dashboard::DashboardError {
    fn from(e: CompareError) -> Self {
        use super::dashboard::DashboardError;
        match e {
            CompareError::Invalid(msg) => DashboardError::BadRequest(msg),
            CompareError::Db(err) => {
                tracing::error!(error = %err, "comparison query failed");
                DashboardError::Internal
            }
        }
    }
}

// ── Sources ───────────────────────────────────────────────────────────────────

/// Everything the builder reads. The handlers take it from `AppState`; the CLI
/// assembles it from settings without constructing an `AppState`.
#[derive(Clone)]
pub struct CompareSources {
    /// Ledger, failure log and the `experiments` table (the variant
    /// dimension reads the experiment through `ExperimentRepository` here).
    pub db: Arc<dyn DatabaseProvider>,
    /// Prompt log, which may live in a separate database.
    pub prompt_db: Arc<dyn DatabaseProvider>,
    pub cost_calc: Arc<CostCalculator>,
}

impl CompareSources {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            db: state.db.clone(),
            prompt_db: state.prompt_db.clone(),
            cost_calc: state.cost_calc.clone(),
        }
    }
}

// ── Result document ───────────────────────────────────────────────────────────

/// Metrics for one arm. Per-request figures are `None` when the arm has no
/// ledger rows, so nothing divides by zero and the page can show a dash.
#[derive(Debug, Clone, Serialize)]
pub struct ArmMetrics {
    pub value: String,
    pub label: String,
    pub requests: i64,
    pub cost_usd: f64,
    pub cost_per_request: Option<f64>,
    pub saved_usd: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub tokens_in_per_request: Option<f64>,
    pub tokens_out_per_request: Option<f64>,
    pub cache_hits: i64,
    pub hit_rate: f64,
    /// Rows in `request_failures` for the arm and window.
    pub failures: i64,
    /// `failures / (requests + failures)`; `0.0` when both are zero.
    pub error_rate: f64,
    pub latency: LatencySummary,
    /// True when any model in the arm has no pricing entry, so `cost_usd` is
    /// incomplete.
    pub unpriced: bool,
    pub unpriced_models: Vec<String>,
    pub by_day: Vec<AttributionBreakdownRow>,
}

/// B minus A. `pct` is `None` when A is zero.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Delta {
    pub abs: f64,
    pub pct: Option<f64>,
}

impl Delta {
    fn between(a: f64, b: f64) -> Delta {
        let abs = b - a;
        let pct = if a == 0.0 { None } else { Some(abs / a * 100.0) };
        Delta { abs, pct }
    }

    fn opt(a: Option<f64>, b: Option<f64>) -> Option<Delta> {
        Some(Delta::between(a?, b?))
    }
}

/// One delta per numeric metric; `None` when either side has no figure.
#[derive(Debug, Clone, Serialize)]
pub struct Deltas {
    pub requests: Option<Delta>,
    pub cost_usd: Option<Delta>,
    pub cost_per_request: Option<Delta>,
    pub tokens_in: Option<Delta>,
    pub tokens_out: Option<Delta>,
    pub tokens_in_per_request: Option<Delta>,
    pub tokens_out_per_request: Option<Delta>,
    pub hit_rate: Option<Delta>,
    pub error_rate: Option<Delta>,
    pub mean_ms: Option<Delta>,
    pub p50_ms: Option<Delta>,
    pub p95_ms: Option<Delta>,
}

impl Deltas {
    fn between(a: &ArmMetrics, b: &ArmMetrics) -> Deltas {
        let f = |v: i64| v as f64;
        let fi = |v: Option<i64>| v.map(f);
        Deltas {
            requests: Some(Delta::between(f(a.requests), f(b.requests))),
            cost_usd: Some(Delta::between(a.cost_usd, b.cost_usd)),
            cost_per_request: Delta::opt(a.cost_per_request, b.cost_per_request),
            tokens_in: Some(Delta::between(f(a.tokens_in), f(b.tokens_in))),
            tokens_out: Some(Delta::between(f(a.tokens_out), f(b.tokens_out))),
            tokens_in_per_request: Delta::opt(a.tokens_in_per_request, b.tokens_in_per_request),
            tokens_out_per_request: Delta::opt(a.tokens_out_per_request, b.tokens_out_per_request),
            hit_rate: Some(Delta::between(a.hit_rate, b.hit_rate)),
            error_rate: Some(Delta::between(a.error_rate, b.error_rate)),
            mean_ms: Delta::opt(a.latency.mean_ms, b.latency.mean_ms),
            p50_ms: Delta::opt(fi(a.latency.p50_ms), fi(b.latency.p50_ms)),
            p95_ms: Delta::opt(fi(a.latency.p95_ms), fi(b.latency.p95_ms)),
        }
    }
}

/// Why the latency and cost denominators differ, per arm.
#[derive(Debug, Clone, Serialize)]
pub struct CoverageArm {
    pub requests: i64,
    pub latency_samples: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Coverage {
    pub a: CoverageArm,
    pub b: CoverageArm,
    /// Reserved for §7.0a; always `null` until pairs are recorded.
    pub incomplete_pairs: Option<i64>,
}

/// The experiment behind a variant comparison (R21): enough for the page and
/// the CLI to say what the arms are and whether their content was stored.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ComparedExperiment {
    pub id: i64,
    pub name: String,
    pub status: ExperimentStatus,
    pub retain_content: bool,
    /// Days after close that retained content is kept; 0 means forever.
    pub content_retention_days: i64,
    /// Whether the prompt log holds the content behind both arms; printed
    /// verbatim by the page and the CLI.
    pub stored_content_note: String,
}

impl ComparedExperiment {
    fn from_experiment(exp: &Experiment) -> Self {
        let stored_content_note = if exp.retain_content {
            let kept = if exp.content_retention_days == 0 {
                "are never purged".to_string()
            } else {
                format!(
                    "are purged {} days after the experiment closes",
                    exp.content_retention_days
                )
            };
            format!(
                "Stored content: experiment {} retains content, so the prompts and responses \
                 behind both arms are in the prompt log and {}.",
                exp.name, kept
            )
        } else {
            format!(
                "Stored content: experiment {} does not retain content; prompts and responses \
                 behind these arms are stored only as the global prompt log settings allow.",
                exp.name
            )
        };
        Self {
            id: exp.id,
            name: exp.name.clone(),
            status: exp.status,
            retain_content: exp.retain_content,
            content_retention_days: exp.content_retention_days,
            stored_content_note,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub dimension: String,
    pub key: Option<String>,
    pub window: String,
    pub start: String,
    pub end: String,
    /// Set for the variant dimension only.
    pub experiment: Option<ComparedExperiment>,
    pub a: ArmMetrics,
    pub b: ArmMetrics,
    pub delta: Deltas,
    pub coverage: Coverage,
    /// Never populated today; see `ttft_note`.
    pub ttft: Option<()>,
    pub ttft_note: &'static str,
    pub caveats: [&'static str; 2],
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Validate `query` and assemble the comparison from the three sources.
///
/// Every query is bounded: window-limited, aggregate-only, and percentiles are
/// single-row offset lookups (see `PromptRepository::latency_summary`).
pub async fn build_comparison(
    sources: &CompareSources,
    query: &CompareQuery,
) -> Result<Comparison, CompareError> {
    let q = query.validate()?;
    let experiment = match q.experiment_id {
        Some(id) => Some(load_experiment(sources, id, &q.a, &q.b).await?),
        None => None,
    };
    let (a, b) = tokio::try_join!(
        arm_metrics(sources, &q.arm_a, &q.a, &q.start, &q.end),
        arm_metrics(sources, &q.arm_b, &q.b, &q.start, &q.end),
    )?;
    let delta = Deltas::between(&a, &b);
    let coverage = Coverage {
        a: CoverageArm { requests: a.requests, latency_samples: a.latency.samples },
        b: CoverageArm { requests: b.requests, latency_samples: b.latency.samples },
        incomplete_pairs: None,
    };
    let quality = if experiment.is_some() { CAVEAT_QUALITY_VARIANT } else { CAVEAT_QUALITY };
    Ok(Comparison {
        dimension: q.dimension,
        key: q.key,
        window: q.window,
        start: q.start,
        end: q.end,
        experiment,
        a,
        b,
        delta,
        coverage,
        ttft: None,
        ttft_note: TTFT_NOTE,
        caveats: [quality, CAVEAT_STREAMING],
    })
}

/// The experiment behind a variant comparison, or the validation error the
/// CLI and the dashboard both report: no such id, or a label it never
/// declared. Labels have passed the charset check, so echoing them is safe.
async fn load_experiment(
    sources: &CompareSources,
    id: i64,
    a: &str,
    b: &str,
) -> Result<ComparedExperiment, CompareError> {
    let exp = ExperimentRepository::get(&*sources.db, id)
        .await?
        .ok_or_else(|| CompareError::Invalid(format!("key: experiment {id} does not exist")))?;
    for (field, label) in [("a", a), ("b", b)] {
        if !exp.variants.contains_key(label) {
            return Err(CompareError::Invalid(format!(
                "{field}: experiment {id} has no variant {label}"
            )));
        }
    }
    Ok(ComparedExperiment::from_experiment(&exp))
}

async fn arm_metrics(
    sources: &CompareSources,
    filter: &ArmFilter,
    value: &str,
    start: &str,
    end: &str,
) -> anyhow::Result<ArmMetrics> {
    let (totals, by_model, by_day, latency, failures) = tokio::try_join!(
        CostRepository::arm_totals(&*sources.db, filter, start, end),
        CostRepository::arm_by_model(&*sources.db, filter, start, end),
        CostRepository::arm_by_day(&*sources.db, filter, start, end),
        PromptRepository::latency_summary(&*sources.prompt_db, filter, start, end),
        FailureRepository::count_for_arm(&*sources.db, filter, start, end),
    )?;

    let per_request = |v: f64| {
        if totals.requests == 0 { None } else { Some(v / totals.requests as f64) }
    };
    let attempts = totals.requests + failures;
    let error_rate = if attempts == 0 { 0.0 } else { failures as f64 / attempts as f64 };
    let unpriced_models: Vec<String> = by_model
        .iter()
        .filter(|row| !sources.cost_calc.has_price(&row.key))
        .map(|row| row.key.clone())
        .collect();

    Ok(ArmMetrics {
        value: value.to_string(),
        label: filter.label(),
        requests: totals.requests,
        cost_usd: totals.cost_usd,
        cost_per_request: per_request(totals.cost_usd),
        saved_usd: totals.saved_usd,
        tokens_in: totals.tokens_in,
        tokens_out: totals.tokens_out,
        tokens_in_per_request: per_request(totals.tokens_in as f64),
        tokens_out_per_request: per_request(totals.tokens_out as f64),
        cache_hits: totals.cache_hits,
        hit_rate: totals.hit_rate(),
        failures,
        error_rate,
        latency,
        unpriced: !unpriced_models.is_empty(),
        unpriced_models,
        by_day,
    })
}

// ── REST API ──────────────────────────────────────────────────────────────────

/// GET /admin/api/compare?dimension=&a=&b=&window=[&key=]
pub async fn get_compare(
    State(state): State<AppState>,
    _session: AdminSession,
    Query(q): Query<CompareQuery>,
) -> Result<Json<Comparison>, ApiError> {
    let sources = CompareSources::from_state(&state);
    Ok(Json(build_comparison(&sources, &q).await?))
}

// ── Dashboard ─────────────────────────────────────────────────────────────────

/// Selection echoed back into the page. Everything is optional so a bare
/// `/admin/compare` renders the pickers with nothing chosen yet.
#[derive(Debug, Default, Deserialize)]
pub struct ComparePageQuery {
    #[serde(default)]
    pub dimension: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub a: String,
    #[serde(default)]
    pub b: String,
    #[serde(default)]
    pub window: String,
}

/// GET /admin/compare — pickers for one dimension plus an empty panel that
/// HTMX fills from `/admin/compare/panels`.
pub async fn get_compare_page(
    State(state): State<AppState>,
    _session: DashboardSession,
    Query(q): Query<ComparePageQuery>,
) -> Result<Html<String>, DashboardError> {
    let dimension = if DIMENSIONS.contains(&q.dimension.as_str()) {
        q.dimension.as_str()
    } else {
        "model"
    };
    let window = if WINDOWS.contains(&q.window.as_str()) {
        q.window.as_str()
    } else {
        "all"
    };
    let db = &*state.db;
    let internal = |_| DashboardError::Internal;

    let mut keys: Vec<String> = Vec::new();
    let mut experiments: Vec<ExperimentOption> = Vec::new();
    let mut key: Option<String> = None;
    let values: Vec<String> = match dimension {
        "model" => CostRepository::distinct_models_in_ledger(db).await.map_err(internal)?,
        "provider" => CostRepository::distinct_providers_in_ledger(db)
            .await
            .map_err(internal)?,
        "tag" => {
            keys = CostRepository::distinct_attribution_tag_keys(db).await.map_err(internal)?;
            // Only a key the ledger actually holds reaches the value query, so
            // an arbitrary key from the URL is never interpolated anywhere.
            let chosen = q.key.trim();
            if keys.iter().any(|k| k == chosen) {
                key = Some(chosen.to_string());
                CostRepository::distinct_attribution_values(db, Some(chosen), FACET_LIMIT)
                    .await
                    .map_err(internal)?
            } else {
                Vec::new()
            }
        }
        "variant" => {
            // Every experiment, closed ones included: a closed experiment is
            // exactly the one whose arms are worth comparing. The key slot
            // submits the id; the arms are the labels the experiment declares.
            let all = ExperimentRepository::list(db, ExperimentStatusFilter::All)
                .await
                .map_err(internal)?;
            let chosen = q.key.trim().parse::<i64>().ok();
            let mut values = Vec::new();
            for exp in &all {
                if chosen == Some(exp.id) {
                    key = Some(exp.id.to_string());
                    values = exp.variants.keys().cloned().collect();
                }
            }
            experiments = all.iter().map(ExperimentOption::from_experiment).collect();
            values
        }
        _ => CostRepository::distinct_recent_correlation_ids(db, FACET_LIMIT)
            .await
            .map_err(internal)?,
    };
    let caveat_quality = if dimension == "variant" { CAVEAT_QUALITY_VARIANT } else { CAVEAT_QUALITY };

    super::dashboard::render(
        "compare.html",
        minijinja::context! {
            sel_dimension => dimension,
            sel_key => key,
            sel_a => q.a,
            sel_b => q.b,
            sel_window => window,
            keys => keys,
            experiments => experiments,
            values => values,
            caveat_quality => caveat_quality,
        },
    )
}

/// One entry of the experiment picker on the page.
#[derive(Debug, Serialize)]
struct ExperimentOption {
    /// The id as the form submits it, so the template compares strings.
    id: String,
    /// The name, with closed experiments marked so they can still be picked
    /// but are not mistaken for live ones.
    text: String,
}

impl ExperimentOption {
    fn from_experiment(exp: &Experiment) -> Self {
        let text = match exp.status {
            ExperimentStatus::Active => exp.name.clone(),
            ExperimentStatus::Closed => format!("{} (closed)", exp.name),
        };
        Self { id: exp.id.to_string(), text }
    }
}

/// One row of the metric table, pre-formatted so the template only prints.
#[derive(Debug, Serialize)]
struct MetricRow {
    label: String,
    a: String,
    b: String,
    delta: String,
}

pub(crate) fn fmt_opt<F: Fn(f64) -> String>(v: Option<f64>, f: F) -> String {
    v.map(f).unwrap_or_else(|| "—".to_string())
}

/// Sign prefix for a delta: `+` for an increase, nothing for zero (the
/// number carries its own `-`). The dashboard and the CLI share it so a zero
/// delta prints the same in both.
pub(crate) fn sign_prefix(v: f64) -> &'static str {
    if v > 0.0 { "+" } else { "" }
}

fn fmt_delta<F: Fn(f64) -> String>(d: &Option<Delta>, f: F) -> String {
    match d {
        None => "—".to_string(),
        Some(d) => {
            let sign = sign_prefix(d.abs);
            match d.pct {
                Some(pct) => format!("{sign}{} ({sign}{pct:.1}%)", f(d.abs)),
                None => format!("{sign}{}", f(d.abs)),
            }
        }
    }
}

fn fmt_count(v: f64) -> String {
    format!("{}", v.round() as i64)
}

pub(crate) fn fmt_ms(v: f64) -> String {
    format!("{:.0} ms", v)
}

fn fmt_rate(v: f64) -> String {
    format!("{:.1}%", v * 100.0)
}

fn fmt_per_request(v: f64) -> String {
    format!("{:.1}", v)
}

fn metric_rows(c: &Comparison) -> Vec<MetricRow> {
    let money = super::templates::fmt_money;
    let (a, b, d) = (&c.a, &c.b, &c.delta);
    let ms = |v: Option<i64>| fmt_opt(v.map(|v| v as f64), fmt_ms);
    vec![
        // Per-request figures first: they are what an experiment is judged on.
        MetricRow {
            label: "Cost per request".into(),
            a: fmt_opt(a.cost_per_request, money),
            b: fmt_opt(b.cost_per_request, money),
            delta: fmt_delta(&d.cost_per_request, money),
        },
        MetricRow {
            label: "Tokens in per request".into(),
            a: fmt_opt(a.tokens_in_per_request, fmt_per_request),
            b: fmt_opt(b.tokens_in_per_request, fmt_per_request),
            delta: fmt_delta(&d.tokens_in_per_request, fmt_per_request),
        },
        MetricRow {
            label: "Tokens out per request".into(),
            a: fmt_opt(a.tokens_out_per_request, fmt_per_request),
            b: fmt_opt(b.tokens_out_per_request, fmt_per_request),
            delta: fmt_delta(&d.tokens_out_per_request, fmt_per_request),
        },
        MetricRow {
            label: "Mean latency".into(),
            a: fmt_opt(a.latency.mean_ms, fmt_ms),
            b: fmt_opt(b.latency.mean_ms, fmt_ms),
            delta: fmt_delta(&d.mean_ms, fmt_ms),
        },
        MetricRow {
            label: "p50 latency".into(),
            a: ms(a.latency.p50_ms),
            b: ms(b.latency.p50_ms),
            delta: fmt_delta(&d.p50_ms, fmt_ms),
        },
        MetricRow {
            label: "p95 latency".into(),
            a: ms(a.latency.p95_ms),
            b: ms(b.latency.p95_ms),
            delta: fmt_delta(&d.p95_ms, fmt_ms),
        },
        MetricRow {
            label: "Cache hit rate".into(),
            a: fmt_rate(a.hit_rate),
            b: fmt_rate(b.hit_rate),
            delta: fmt_delta(&d.hit_rate, fmt_rate),
        },
        MetricRow {
            label: "Error rate".into(),
            a: fmt_rate(a.error_rate),
            b: fmt_rate(b.error_rate),
            delta: fmt_delta(&d.error_rate, fmt_rate),
        },
        // Totals second: they mostly reflect how much traffic each arm saw.
        MetricRow {
            label: "Requests".into(),
            a: a.requests.to_string(),
            b: b.requests.to_string(),
            delta: fmt_delta(&d.requests, fmt_count),
        },
        MetricRow {
            label: "Total cost".into(),
            a: money(a.cost_usd),
            b: money(b.cost_usd),
            delta: fmt_delta(&d.cost_usd, money),
        },
        MetricRow {
            label: "Saved by cache".into(),
            a: money(a.saved_usd),
            b: money(b.saved_usd),
            delta: "—".into(),
        },
        MetricRow {
            label: "Tokens in".into(),
            a: a.tokens_in.to_string(),
            b: b.tokens_in.to_string(),
            delta: fmt_delta(&d.tokens_in, fmt_count),
        },
        MetricRow {
            label: "Tokens out".into(),
            a: a.tokens_out.to_string(),
            b: b.tokens_out.to_string(),
            delta: fmt_delta(&d.tokens_out, fmt_count),
        },
        MetricRow {
            label: "Cache hits".into(),
            a: a.cache_hits.to_string(),
            b: b.cache_hits.to_string(),
            delta: "—".into(),
        },
        MetricRow {
            label: "Failures".into(),
            a: a.failures.to_string(),
            b: b.failures.to_string(),
            delta: "—".into(),
        },
    ]
}

/// Chart payloads. The chart legend shows the raw arm value, which is what
/// the operator picked; the metric table carries the full `label`.
fn chart_json(c: &Comparison) -> (String, String, String) {
    let to_json = |v: serde_json::Value| serde_json::to_string(&v).unwrap_or_else(|_| "{}".into());
    let bars = |m: &ArmMetrics| {
        serde_json::json!({
            "label": m.value,
            "requests": m.requests,
            "cost_per_request": m.cost_per_request,
            "tokens_in_per_request": m.tokens_in_per_request,
            "tokens_out_per_request": m.tokens_out_per_request,
            "mean_ms": m.latency.mean_ms,
        })
    };
    let daily = |m: &ArmMetrics| {
        serde_json::json!({
            "label": m.value,
            "requests": m.requests,
            "series": m.by_day.iter().map(|row| serde_json::json!({
                "day": row.key,
                "cost_usd": row.totals.cost_usd,
            })).collect::<Vec<_>>(),
        })
    };
    let latency = |m: &ArmMetrics| {
        serde_json::json!({
            "label": m.value,
            "samples": m.latency.samples,
            "p50_ms": m.latency.p50_ms,
            "p95_ms": m.latency.p95_ms,
        })
    };
    (
        to_json(serde_json::json!({ "a": bars(&c.a), "b": bars(&c.b) })),
        to_json(serde_json::json!({ "a": daily(&c.a), "b": daily(&c.b) })),
        to_json(serde_json::json!({ "a": latency(&c.a), "b": latency(&c.b) })),
    )
}

/// GET /admin/compare/panels — the comparison itself, swapped into the page.
///
/// A rejected query renders as an inline message rather than an error page:
/// the pickers are still on screen and the operator only needs to fix one.
pub async fn get_compare_panels(
    State(state): State<AppState>,
    _session: DashboardSession,
    Query(q): Query<CompareQuery>,
) -> Result<Html<String>, DashboardError> {
    let render_message = |message: String, hint: bool| {
        super::dashboard::render(
            "compare_panels.html",
            minijinja::context! { message => message, hint => hint },
        )
    };
    if q.a.trim().is_empty() || q.b.trim().is_empty() {
        return render_message("Choose two arms to compare.".to_string(), true);
    }
    let sources = CompareSources::from_state(&state);
    let comparison = match build_comparison(&sources, &q).await {
        Ok(c) => c,
        Err(CompareError::Invalid(msg)) => return render_message(msg, false),
        Err(e) => return Err(e.into()),
    };
    let rows = metric_rows(&comparison);
    let (bars_json, daily_json, latency_json) = chart_json(&comparison);
    super::dashboard::render(
        "compare_panels.html",
        minijinja::context! {
            cmp => minijinja::Value::from_serialize(&comparison),
            rows => minijinja::Value::from_serialize(&rows),
            bars_json => bars_json,
            daily_json => daily_json,
            latency_json => latency_json,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(dimension: &str, key: &str, a: &str, b: &str, window: &str) -> CompareQuery {
        CompareQuery {
            dimension: dimension.into(),
            key: key.into(),
            a: a.into(),
            b: b.into(),
            window: window.into(),
        }
    }

    #[test]
    fn validate_maps_each_dimension_to_its_arm_filter() {
        let v = q("model", "", "x", "y", "all").validate().unwrap();
        assert_eq!(v.arm_a, ArmFilter::Model("x".into()));
        let v = q("provider", "", "x", "y", "all").validate().unwrap();
        assert_eq!(v.arm_b, ArmFilter::Provider("y".into()));
        let v = q("tag", "arm", " x ", "y", "all").validate().unwrap();
        assert_eq!(
            v.arm_a,
            ArmFilter::Attribution(AttributionFilter::Tag { key: "arm".into(), value: "x".into() })
        );
        assert_eq!(v.key.as_deref(), Some("arm"));
        let v = q("run", "ignored", "x", "y", "weekly").validate().unwrap();
        assert_eq!(v.arm_a, ArmFilter::Attribution(AttributionFilter::CorrelationId("x".into())));
        assert_eq!(v.key, None);
        assert_eq!(v.experiment_id, None);
        assert_eq!(v.window, "weekly");
        let v = q("variant", " 7 ", "control", "candidate.v2", "all").validate().unwrap();
        assert_eq!(v.arm_a, ArmFilter::Variant { experiment_id: 7, variant: "control".into() });
        assert_eq!(v.arm_b, ArmFilter::Variant { experiment_id: 7, variant: "candidate.v2".into() });
        assert_eq!(v.key.as_deref(), Some("7"));
        assert_eq!(v.experiment_id, Some(7));
    }

    #[test]
    fn validate_variant_checks_the_id_and_the_label_charset_without_a_database() {
        let err = |query: CompareQuery| query.validate().unwrap_err().to_string();
        for key in ["", "abc", "0", "-3", "1.5", "99999999999999999999"] {
            assert!(err(q("variant", key, "x", "y", "all")).starts_with("key"), "key {key:?}");
        }
        assert!(err(q("variant", "1", "bad label", "y", "all")).starts_with("a "));
        assert!(err(q("variant", "1", "x", "b/y", "all")).starts_with("b "));
        assert!(err(q("variant", "1", &"x".repeat(65), "y", "all")).starts_with("a "));
        // The label is not echoed before it passes the charset check.
        assert!(!err(q("variant", "1", "<script>", "y", "all")).contains("<script>"));
    }

    #[test]
    fn validate_rejects_bad_input_naming_the_field() {
        let err = |query: CompareQuery| query.validate().unwrap_err().to_string();
        assert!(err(q("pair", "", "x", "y", "all")).starts_with("dimension"));
        assert!(err(q("model", "", "", "y", "all")).starts_with("a "));
        assert!(err(q("model", "", "x", " ", "all")).starts_with("b "));
        assert!(err(q("model", "", "x", "x", "all")).starts_with("a and b"));
        assert!(err(q("tag", "", "x", "y", "all")).starts_with("key"));
        assert!(err(q("tag", "a b", "x", "y", "all")).contains("tag key"));
        assert!(err(q("model", "", "x", "y", "hourly")).starts_with("window"));
    }

    #[test]
    fn stored_content_note_says_whether_content_is_kept_and_for_how_long() {
        let exp = |retain: bool, days: i64| Experiment {
            id: 3,
            name: "exp".into(),
            variants: Default::default(),
            allowed_user_ids: vec![],
            status: ExperimentStatus::Active,
            feed_learning: false,
            expires_at: 0,
            created_at: String::new(),
            closed_at: None,
            retain_content: retain,
            content_retention_days: days,
        };
        let note = |retain, days| ComparedExperiment::from_experiment(&exp(retain, days)).stored_content_note;
        assert!(note(false, 0).contains("does not retain content"), "{}", note(false, 0));
        assert!(note(true, 0).contains("never purged"), "{}", note(true, 0));
        assert!(note(true, 30).contains("30 days"), "{}", note(true, 30));
        assert_eq!(ComparedExperiment::from_experiment(&exp(true, 30)).status, ExperimentStatus::Active);
    }

    #[test]
    fn delta_is_b_minus_a_with_no_percentage_from_zero() {
        assert_eq!(Delta::between(4.0, 1.0), Delta { abs: -3.0, pct: Some(-75.0) });
        assert_eq!(Delta::between(0.0, 2.0), Delta { abs: 2.0, pct: None });
        assert_eq!(Delta::opt(None, Some(1.0)), None);
    }
}
