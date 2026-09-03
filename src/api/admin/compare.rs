//! Side-by-side comparison of two experiment arms (spec §7b).
//!
//! An arm is a slice of recorded traffic — one model, one provider, one tag
//! value or one correlation id. The router never assigns arms; a client forms
//! them by choosing a model per arm and tagging each request, and this module
//! partitions what the ledger, the prompt log and the failure log recorded.
//!
//! One builder serves three consumers: the JSON endpoint here, the dashboard
//! panels, and `modelrouter report compare`. The dashboard and CLI must show
//! the same numbers, so neither computes anything of its own.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::app::{AppState, DatabaseProvider};
use crate::api::error::ApiError;
use crate::db::repositories::costs::{
    ArmFilter, AttributionBreakdownRow, AttributionFilter, CostRepository,
};
use crate::db::repositories::failures::FailureRepository;
use crate::db::repositories::prompts::{LatencySummary, PromptRepository};
use crate::router::cost::CostCalculator;

use super::attribution::window_range;
use super::auth::AdminSession;

/// Statements every comparison carries, in order. The page and the CLI print
/// them verbatim; tests pin their presence, not their wording.
pub const CAVEAT_QUALITY: &str = "This comparison has no quality column. A difference in cost or \
    latency is not evidence of a difference in answer quality.";
pub const CAVEAT_STREAMING: &str = "Streamed responses record estimated or zero tokens and, on the \
    messages API, a placeholder latency; they are indistinguishable from measured rows here. \
    Send experiment traffic with stream: false.";
pub const TTFT_NOTE: &str =
    "Time to first token is not recorded by the router today, so it cannot be compared.";

pub const DIMENSIONS: [&str; 4] = ["model", "provider", "tag", "run"];
pub const WINDOWS: [&str; 4] = ["all", "daily", "weekly", "monthly"];

// ── Query ─────────────────────────────────────────────────────────────────────

/// Query shared by the REST endpoint, the dashboard panels and the CLI.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompareQuery {
    #[serde(default)]
    pub dimension: String,
    /// Tag key; required when `dimension = tag`, ignored otherwise.
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub a: String,
    #[serde(default)]
    pub b: String,
    #[serde(default = "default_window")]
    pub window: String,
}

fn default_window() -> String {
    "monthly".to_string()
}

/// A query that has passed validation: two arm filters and a window.
#[derive(Debug, Clone)]
pub struct ValidatedQuery {
    pub dimension: String,
    pub key: Option<String>,
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
            _ => None,
        };

        let arm = |value: &str| match dimension {
            "model" => ArmFilter::Model(value.to_string()),
            "provider" => ArmFilter::Provider(value.to_string()),
            "tag" => ArmFilter::Attribution(AttributionFilter::Tag {
                key: key.clone().unwrap_or_default(),
                value: value.to_string(),
            }),
            _ => ArmFilter::Attribution(AttributionFilter::CorrelationId(value.to_string())),
        };
        let (arm_a, arm_b) = (arm(a), arm(b));
        let (start, end) = window_range(window);
        Ok(ValidatedQuery {
            dimension: dimension.to_string(),
            key,
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
    /// Ledger and failure log.
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

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub dimension: String,
    pub key: Option<String>,
    pub window: String,
    pub start: String,
    pub end: String,
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
    Ok(Comparison {
        dimension: q.dimension,
        key: q.key,
        window: q.window,
        start: q.start,
        end: q.end,
        a,
        b,
        delta,
        coverage,
        ttft: None,
        ttft_note: TTFT_NOTE,
        caveats: [CAVEAT_QUALITY, CAVEAT_STREAMING],
    })
}

async fn arm_metrics(
    sources: &CompareSources,
    filter: &ArmFilter,
    value: &str,
    start: &str,
    end: &str,
) -> anyhow::Result<ArmMetrics> {
    let totals = CostRepository::arm_totals(&*sources.db, filter, start, end).await?;
    let by_model = CostRepository::arm_by_model(&*sources.db, filter, start, end).await?;
    let by_day = CostRepository::arm_by_day(&*sources.db, filter, start, end).await?;
    let latency = PromptRepository::latency_summary(&*sources.prompt_db, filter, start, end).await?;
    let failures = FailureRepository::count_for_arm(&*sources.db, filter, start, end).await?;

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
        assert_eq!(v.window, "weekly");
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
    fn delta_is_b_minus_a_with_no_percentage_from_zero() {
        assert_eq!(Delta::between(4.0, 1.0), Delta { abs: -3.0, pct: Some(-75.0) });
        assert_eq!(Delta::between(0.0, 2.0), Delta { abs: 2.0, pct: None });
        assert_eq!(Delta::opt(None, Some(1.0)), None);
    }
}
