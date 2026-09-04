//! Admin API for controlled experiments (spec §7a).
//!
//! An experiment is created once, with every variant's target resolved and
//! pinned to a `provider/model` pair at that moment, and later closed; nothing
//! else about a row ever changes. Creation is gated so an experiment can only
//! send traffic somewhere whose cost is known: each target expression must
//! resolve without falling through to `default_model`, name a configured
//! provider, not be a load balancer pool, and have a pricing entry. Every
//! write is recorded in the audit log and reloads the live
//! [`crate::router::experiments::ExperimentRegistry`], so the next request
//! can bind without a restart.
//!
//! The body is walked by hand rather than deserialised into a struct so a
//! missing or mistyped field is a 400 naming that field, and so `expires_at`
//! and `content_retention_days` are never defaulted: the caller must write
//! `0` to mean never.
//!
//! Results (R14) are one document assembled by [`build_results`] from the
//! ledger, the prompt log, the failure log and the outcome table, the way
//! `compare::build_comparison` is shared by the JSON endpoint, the dashboard
//! and the CLI; nothing downstream computes a figure of its own.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::audit::audit;
use crate::api::{
    admin::auth::{AdminSession, SuperAdminSession},
    app::{AppState, DatabaseProvider},
    error::ApiError,
};
use crate::db::models::{Experiment, ExperimentVariants, NewExperiment, RunOutcome, VariantTarget};
use crate::db::repositories::costs::{
    ArmFilter, CostRepository, ExperimentRunKey, ExperimentRunRow,
};
use crate::db::repositories::experiments::{ExperimentRepository, ExperimentStatusFilter};
use crate::db::repositories::failures::FailureRepository;
use crate::db::repositories::outcomes::OutcomeRepository;
use crate::db::repositories::prompts::{LatencySummary, PromptRepository};
use crate::db::repositories::users::UserRepository;
use crate::router::cost::CostCalculator;
use crate::router::experiments::{is_valid_label, MAX_LABEL_LEN};

/// Bounds on a create request. Labels are further limited to
/// [`MAX_LABEL_LEN`] so they always fit the request header.
const MAX_NAME_LEN: usize = 128;
const MIN_VARIANTS: usize = 2;
const MAX_VARIANTS: usize = 16;
const MAX_OVERLAY_ENTRIES: usize = 32;
const MAX_EXPR_LEN: usize = 128;
const MAX_RETENTION_DAYS: i64 = 3650;
/// Upper bound on `allowed_user_ids`; each id costs one lookup at creation.
const MAX_ALLOWED_USERS: usize = 64;

// ── Body validation ───────────────────────────────────────────────────────────

/// Everything a create request must say, after validation but before the
/// targets are resolved. `variants` still holds the raw expressions.
#[derive(Debug)]
struct ParsedCreate {
    name: String,
    variants: BTreeMap<String, BTreeMap<String, String>>,
    allowed_user_ids: Vec<i64>,
    feed_learning: bool,
    expires_at: i64,
    retain_content: bool,
    content_retention_days: i64,
}


fn require<'a>(body: &'a Value, field: &str) -> Result<&'a Value, String> {
    match body.get(field) {
        None | Some(Value::Null) => Err(format!("{field} is required")),
        Some(v) => Ok(v),
    }
}

fn require_bool(body: &Value, field: &str) -> Result<bool, String> {
    require(body, field)?
        .as_bool()
        .ok_or_else(|| format!("{field} must be a boolean"))
}

/// `expires_at`: an RFC3339 timestamp in the future, or the number `0` for
/// never. Any other number is refused rather than guessed at as an epoch.
fn parse_expires_at(body: &Value, now: chrono::DateTime<chrono::Utc>) -> Result<i64, String> {
    match require(body, "expires_at")? {
        Value::Number(n) if n.as_i64() == Some(0) => Ok(0),
        Value::Number(_) => Err("expires_at must be an RFC3339 timestamp or 0 (never)".to_string()),
        Value::String(s) => {
            let ts = chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|_| "expires_at must be an RFC3339 timestamp or 0 (never)".to_string())?;
            if ts.with_timezone(&chrono::Utc) <= now {
                return Err("expires_at must be in the future".to_string());
            }
            Ok(ts.timestamp())
        }
        _ => Err("expires_at must be an RFC3339 timestamp or 0 (never)".to_string()),
    }
}

/// Walk the body field by field. Shape and bounds only; existence checks
/// (name uniqueness, user ids, target resolution) need the state and happen
/// in the handler.
fn parse_create(body: &Value, now: chrono::DateTime<chrono::Utc>) -> Result<ParsedCreate, String> {
    if !body.is_object() {
        return Err("body must be a JSON object".to_string());
    }

    let name = require(body, "name")?
        .as_str()
        .ok_or_else(|| "name must be a string".to_string())?
        .trim()
        .to_string();
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
        return Err(format!("name must be 1-{MAX_NAME_LEN} characters"));
    }

    let raw_variants = require(body, "variants")?
        .as_object()
        .ok_or_else(|| "variants must be an object of label -> overlay".to_string())?;
    if raw_variants.len() < MIN_VARIANTS || raw_variants.len() > MAX_VARIANTS {
        return Err(format!(
            "variants must have {MIN_VARIANTS}-{MAX_VARIANTS} entries, got {}",
            raw_variants.len()
        ));
    }
    // A JSON object cannot repeat a key, but serde_json's map keeps the last
    // duplicate silently; distinctness is re-checked as the BTreeMap fills.
    let mut variants: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (label, overlay) in raw_variants {
        if !is_valid_label(label) {
            return Err(format!(
                "variants: label '{label}' must match [A-Za-z0-9_.-]{{1,{MAX_LABEL_LEN}}}"
            ));
        }
        let overlay = overlay.as_object().ok_or_else(|| {
            format!("variants: variant '{label}' must be an object of requested model -> target")
        })?;
        if overlay.len() > MAX_OVERLAY_ENTRIES {
            return Err(format!(
                "variants: variant '{label}' has {} entries; at most {MAX_OVERLAY_ENTRIES} allowed",
                overlay.len()
            ));
        }
        let mut pinned = BTreeMap::new();
        for (key, target) in overlay {
            if key.is_empty() || key.chars().count() > MAX_EXPR_LEN {
                return Err(format!(
                    "variants: variant '{label}' key '{key}' must be 1-{MAX_EXPR_LEN} characters"
                ));
            }
            let target = target.as_str().ok_or_else(|| {
                format!("variants: variant '{label}' key '{key}' target must be a string")
            })?;
            if target.is_empty() || target.chars().count() > MAX_EXPR_LEN {
                return Err(format!(
                    "variants: variant '{label}' key '{key}' target must be 1-{MAX_EXPR_LEN} characters"
                ));
            }
            pinned.insert(key.clone(), target.to_string());
        }
        if variants.insert(label.clone(), pinned).is_some() {
            return Err(format!("variants: label '{label}' appears more than once"));
        }
    }

    let expires_at = parse_expires_at(body, now)?;

    let content_retention_days = require(body, "content_retention_days")?
        .as_i64()
        .ok_or_else(|| "content_retention_days must be an integer".to_string())?;
    if !(0..=MAX_RETENTION_DAYS).contains(&content_retention_days) {
        return Err(format!(
            "content_retention_days must be 0-{MAX_RETENTION_DAYS} (0 = forever)"
        ));
    }

    let retain_content = require_bool(body, "retain_content")?;
    if retain_content && expires_at == 0 {
        return Err(
            "retain_content: true requires expires_at to be set; an experiment that never \
             expires cannot retain content"
                .to_string(),
        );
    }

    let feed_learning = match body.get("feed_learning") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| "feed_learning must be a boolean".to_string())?,
    };

    let allowed_user_ids = match body.get("allowed_user_ids") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => {
            let arr = v
                .as_array()
                .ok_or_else(|| "allowed_user_ids must be an array of integers".to_string())?;
            if arr.len() > MAX_ALLOWED_USERS {
                return Err(format!(
                    "allowed_user_ids must have at most {MAX_ALLOWED_USERS} entries"
                ));
            }
            let mut ids = Vec::with_capacity(arr.len());
            for item in arr {
                let id = item
                    .as_i64()
                    .ok_or_else(|| "allowed_user_ids must be an array of integers".to_string())?;
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            ids
        }
    };

    Ok(ParsedCreate {
        name,
        variants,
        allowed_user_ids,
        feed_learning,
        expires_at,
        retain_content,
        content_retention_days,
    })
}

// ── Pricing gate ──────────────────────────────────────────────────────────────

/// Resolve and pin one overlay target, refusing anything whose cost could
/// not be accounted for. The error names the variant, key and target and
/// says which check failed.
fn gate_target(state: &AppState, label: &str, key: &str, expr: &str) -> Result<VariantTarget, String> {
    let at = format!("variants: variant '{label}' key '{key}' target '{expr}'");

    if state.load_balancer.is_pool(expr) {
        return Err(format!(
            "{at} is a load balancer pool; an experiment must pin one provider/model"
        ));
    }

    let res = state.router.resolve_detailed(expr);
    if res.substituted {
        return Err(format!(
            "{at} is not an alias or provider/model and would be substituted with the default model"
        ));
    }
    if state.provider_registry.get(&res.provider).is_err() {
        return Err(format!("{at} resolves to unconfigured provider '{}'", res.provider));
    }
    let pinned = format!("{}/{}", res.provider, res.model);
    if !state.cost_calc.has_price(&pinned) {
        return Err(format!("{at} resolves to '{pinned}', which has no pricing entry"));
    }

    Ok(VariantTarget {
        target: expr.to_string(),
        provider: res.provider,
        model: res.model,
    })
}

/// Run the gate over every overlay entry, producing the pinned variants.
fn gate_variants(
    state: &AppState,
    raw: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<ExperimentVariants, String> {
    let mut out: ExperimentVariants = BTreeMap::new();
    for (label, overlay) in raw {
        let mut pinned = BTreeMap::new();
        for (key, expr) in overlay {
            pinned.insert(key.clone(), gate_target(state, label, key, expr)?);
        }
        out.insert(label.clone(), pinned);
    }
    Ok(out)
}

// ── Audit and refresh ─────────────────────────────────────────────────────────

/// The row as recorded in the audit log: the stored shape, except that the
/// two zero-means-never fields are spelled out so an auditor never has to
/// know the convention.
fn audit_row(exp: &Experiment) -> Value {
    let mut v = serde_json::to_value(exp).unwrap_or(Value::Null);
    if exp.expires_at == 0 {
        v["expires_at"] = json!("never");
    }
    if exp.content_retention_days == 0 {
        v["content_retention_days"] = json!("never");
    }
    v
}

/// Reload the live registry after a write. The write has already committed,
/// so a failed reload is logged rather than surfaced: the lifecycle tick will
/// retry, and the previous snapshot stays in place until then.
async fn refresh_registry(state: &AppState) {
    if let Err(e) = state.experiments.load_from(&*state.db).await {
        tracing::warn!(error = %e, "failed to reload experiment registry after write");
    }
}

fn parse_id(id: &str) -> Result<i64, ApiError> {
    id.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| ApiError::InvalidRequest(format!("invalid experiment id: {id}")))
}

fn is_unique_violation(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<sqlx::Error>(),
        Some(sqlx::Error::Database(db)) if db.is_unique_violation()
    )
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// POST /admin/api/experiments
pub async fn create_experiment_api(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let now = chrono::Utc::now();
    let parsed = parse_create(&body, now).map_err(ApiError::InvalidRequest)?;

    for id in &parsed.allowed_user_ids {
        let known = UserRepository::find_by_id(&*state.db, *id)
            .await
            .map_err(|_| ApiError::Internal)?;
        if known.is_none() {
            return Err(ApiError::InvalidRequest(format!(
                "allowed_user_ids: no user with id {id}"
            )));
        }
    }

    let variants = gate_variants(&state, &parsed.variants).map_err(ApiError::InvalidRequest)?;

    let name = parsed.name;
    let row = ExperimentRepository::create(
        &*state.db,
        NewExperiment {
            name: name.clone(),
            variants,
            allowed_user_ids: parsed.allowed_user_ids,
            feed_learning: parsed.feed_learning,
            expires_at: parsed.expires_at,
            retain_content: parsed.retain_content,
            content_retention_days: parsed.content_retention_days,
        },
    )
    .await
    .map_err(|e| {
        // `experiments.name` is UNIQUE; the constraint is the duplicate check.
        if is_unique_violation(&e) {
            return ApiError::InvalidRequest(format!("name '{name}' is already taken"));
        }
        tracing::error!(error = %e, "failed to create experiment");
        ApiError::Internal
    })?;

    refresh_registry(&state).await;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "experiment.create",
        Some(format!("experiment:{}", row.id)),
        None,
        Some(audit_row(&row).to_string()),
    )
    .await;

    Ok((StatusCode::CREATED, Json(row)))
}

#[derive(Deserialize)]
pub struct ListExperimentsQuery {
    pub status: Option<String>,
}

/// GET /admin/api/experiments?status=active|closed|all (default active)
pub async fn list_experiments_api(
    State(state): State<AppState>,
    _session: AdminSession,
    Query(query): Query<ListExperimentsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let filter = match query.status.as_deref().map(str::trim) {
        None | Some("") | Some("active") => ExperimentStatusFilter::Active,
        Some("closed") => ExperimentStatusFilter::Closed,
        Some("all") => ExperimentStatusFilter::All,
        Some(other) => {
            return Err(ApiError::InvalidRequest(format!(
                "status must be active, closed or all, got '{other}'"
            )))
        }
    };
    let rows = ExperimentRepository::list(&*state.db, filter)
        .await
        .map_err(|_| ApiError::Internal)?;
    Ok(Json(json!({ "experiments": rows })))
}

/// GET /admin/api/experiments/:id
pub async fn get_experiment_api(
    State(state): State<AppState>,
    _session: AdminSession,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = parse_id(&id)?;
    let row = ExperimentRepository::get(&*state.db, id)
        .await
        .map_err(|_| ApiError::Internal)?
        .ok_or_else(|| ApiError::InvalidRequest(format!("no experiment with id {id}")))?;
    Ok(Json(row))
}

/// POST /admin/api/experiments/:id/close
pub async fn close_experiment_api(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = parse_id(&id)?;
    let before = ExperimentRepository::get(&*state.db, id)
        .await
        .map_err(|_| ApiError::Internal)?
        .ok_or_else(|| ApiError::InvalidRequest(format!("no experiment with id {id}")))?;
    if before.closed_at.is_some() {
        return Err(ApiError::InvalidRequest(format!(
            "experiment {id} is already closed"
        )));
    }

    let closed_at = chrono::Utc::now().to_rfc3339();
    let changed = ExperimentRepository::close(&*state.db, id, &closed_at)
        .await
        .map_err(|_| ApiError::Internal)?;
    if !changed {
        // Lost a race with the lifecycle tick or another operator.
        return Err(ApiError::InvalidRequest(format!(
            "experiment {id} is already closed"
        )));
    }
    let after = ExperimentRepository::get(&*state.db, id)
        .await
        .map_err(|_| ApiError::Internal)?
        .ok_or(ApiError::Internal)?;

    refresh_registry(&state).await;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "experiment.close",
        Some(format!("experiment:{id}")),
        Some(json!({ "status": before.status, "closed_at": before.closed_at }).to_string()),
        Some(json!({ "status": after.status, "closed_at": after.closed_at }).to_string()),
    )
    .await;

    Ok(Json(after))
}

// ── Results (spec §7a, R14) ───────────────────────────────────────────────────

/// Page bounds for the per-run list.
pub const DEFAULT_RUN_LIMIT: i64 = 200;
pub const MAX_RUN_LIMIT: i64 = 1000;

/// Window handed to `latency_summary` for a whole experiment: every row.
/// `created_at` is RFC3339 text, so string bounds order correctly.
const ALL_TIME_START: &str = "1970-01-01T00:00:00Z";
const ALL_TIME_END: &str = "9999-12-31T23:59:59Z";

/// Everything the results builder reads. The handler takes it from
/// `AppState`; the CLI assembles it from settings without an `AppState`.
#[derive(Clone)]
pub struct ExperimentSources {
    /// Experiments, ledger, failure log and outcomes.
    pub db: Arc<dyn DatabaseProvider>,
    /// Prompt log, which may live in a separate database.
    pub prompt_db: Arc<dyn DatabaseProvider>,
    pub cost_calc: Arc<CostCalculator>,
}

impl ExperimentSources {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            db: state.db.clone(),
            prompt_db: state.prompt_db.clone(),
            cost_calc: state.cost_calc.clone(),
        }
    }
}

/// One page of the run list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunPage {
    pub limit: i64,
    pub offset: i64,
}

impl Default for RunPage {
    fn default() -> Self {
        RunPage { limit: DEFAULT_RUN_LIMIT, offset: 0 }
    }
}

impl RunPage {
    /// Parse the raw query values; an absent or empty value takes the
    /// default, anything else must be an integer in range. Errors name the
    /// field so the endpoint's 400 can repeat them verbatim.
    pub fn parse(limit: Option<&str>, offset: Option<&str>) -> Result<RunPage, String> {
        let limit = match limit.map(str::trim) {
            None | Some("") => DEFAULT_RUN_LIMIT,
            Some(raw) => raw
                .parse::<i64>()
                .ok()
                .filter(|n| (1..=MAX_RUN_LIMIT).contains(n))
                .ok_or_else(|| format!("limit must be an integer between 1 and {MAX_RUN_LIMIT}"))?,
        };
        let offset = match offset.map(str::trim) {
            None | Some("") => 0,
            Some(raw) => raw
                .parse::<i64>()
                .ok()
                .filter(|n| *n >= 0)
                .ok_or_else(|| "offset must be an integer of at least 0".to_string())?,
        };
        Ok(RunPage { limit, offset })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResultsError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Db(#[from] anyhow::Error),
}

impl From<ResultsError> for ApiError {
    fn from(e: ResultsError) -> Self {
        match e {
            ResultsError::Invalid(msg) => ApiError::InvalidRequest(msg),
            ResultsError::Db(err) => {
                tracing::error!(error = %err, "experiment results query failed");
                ApiError::Internal
            }
        }
    }
}

impl From<ResultsError> for super::dashboard::DashboardError {
    fn from(e: ResultsError) -> Self {
        use super::dashboard::DashboardError;
        match e {
            ResultsError::Invalid(msg) => DashboardError::BadRequest(msg),
            ResultsError::Db(err) => {
                tracing::error!(error = %err, "experiment results query failed");
                DashboardError::Internal
            }
        }
    }
}

/// Prompt and completion tokens, with their sum for convenience.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct TokenTotals {
    pub prompt: i64,
    pub completion: i64,
    pub total: i64,
}

impl TokenTotals {
    fn new(prompt: i64, completion: i64) -> Self {
        TokenTotals { prompt, completion, total: prompt + completion }
    }

    fn add(&mut self, other: TokenTotals) {
        self.prompt += other.prompt;
        self.completion += other.completion;
        self.total += other.total;
    }
}

/// Latency of one run: only rows with a positive `latency_ms` count, the
/// [`LatencySummary`] rule. Absent (`null`) when there are no samples.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RunLatency {
    pub samples: i64,
    pub mean_ms: f64,
}

/// Outcome feedback aggregated over a set of runs. Rates and means are
/// `None` rather than zero when nothing was reported, so an arm with no
/// feedback is not mistaken for one that always failed.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct OutcomeStats {
    /// Runs with any outcome reported.
    pub reported: i64,
    pub success: i64,
    pub failure: i64,
    /// `success / reported`.
    pub success_rate: Option<f64>,
    pub mean_score: Option<f64>,
    pub score_samples: i64,
    pub mean_rating: Option<f64>,
    pub rating_samples: i64,
}

impl OutcomeStats {
    fn add(&mut self, o: &RunOutcome) {
        self.reported += 1;
        if o.outcome == "success" {
            self.success += 1;
        } else {
            self.failure += 1;
        }
        if let Some(score) = o.score {
            self.mean_score = Some(running_mean(self.mean_score, self.score_samples, score));
            self.score_samples += 1;
        }
        if let Some(rating) = o.rating {
            self.mean_rating =
                Some(running_mean(self.mean_rating, self.rating_samples, rating as f64));
            self.rating_samples += 1;
        }
        self.success_rate = Some(self.success as f64 / self.reported as f64);
    }
}

fn running_mean(mean: Option<f64>, n: i64, value: f64) -> f64 {
    let mean = mean.unwrap_or(0.0);
    mean + (value - mean) / (n as f64 + 1.0)
}

/// One model's share of a variant's stamped requests.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VariantModelMetrics {
    pub model: String,
    pub requests: i64,
    pub cost_usd: f64,
    pub saved_usd: f64,
    pub tokens: TokenTotals,
    pub estimated_rows: i64,
    /// No pricing entry, so `cost_usd` was recorded as zero.
    pub unpriced: bool,
}

/// Figures divided by the variant's run count; absent when it has no runs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PerRunFigures {
    pub turns: f64,
    pub cost_usd: f64,
    pub tokens: f64,
    pub span_secs: f64,
}

/// Figures divided by the variant's request count; absent when it has none.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PerRequestFigures {
    pub cost_usd: f64,
    pub tokens_in: f64,
    pub tokens_out: f64,
}

/// Aggregates for one variant.
///
/// Attribution follows KTD7: run-level figures (`runs`, `mixed_runs`,
/// `turns`, `unbound_requests`, span, outcomes) belong to the variant of the
/// run's earliest stamped row; request-level figures (`requests`, cost,
/// tokens, `estimated_rows`, `failures`, latency, the model breakdown) belong
/// to the variant stamped on each row. The request-level columns therefore
/// always sum to the experiment's totals; a mixed run's figures are split.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VariantResults {
    pub label: String,
    pub runs: i64,
    /// Runs attributed here that were also seen under another variant.
    pub mixed_runs: i64,
    pub requests: i64,
    /// Ledger rows sharing one of this variant's runs' keys with no
    /// experiment id — turns sent without the header. Counted, never merged.
    pub unbound_requests: i64,
    /// Stamped requests of the runs attributed to this variant.
    pub turns: i64,
    pub cost_usd: f64,
    pub saved_usd: f64,
    pub tokens: TokenTotals,
    /// Rows whose token counts were estimated locally rather than reported.
    pub estimated_rows: i64,
    pub failures: i64,
    /// Latency over the variant's prompt rows; `null` with `latency_samples`
    /// of 0 when none carry a measurement (prompt rows are written only
    /// where storage or retention allows).
    pub latency: Option<LatencySummary>,
    pub latency_samples: i64,
    pub per_run: Option<PerRunFigures>,
    pub per_request: Option<PerRequestFigures>,
    pub models: Vec<VariantModelMetrics>,
    /// True when any model in the variant has no pricing entry.
    pub unpriced: bool,
    pub unpriced_models: Vec<String>,
    pub outcomes: OutcomeStats,
    /// Sum of the attributed runs' spans; feeds `per_run.span_secs`.
    #[serde(skip)]
    span_secs: f64,
}

impl VariantResults {
    fn empty(label: &str) -> Self {
        VariantResults {
            label: label.to_string(),
            runs: 0,
            mixed_runs: 0,
            requests: 0,
            unbound_requests: 0,
            turns: 0,
            cost_usd: 0.0,
            saved_usd: 0.0,
            tokens: TokenTotals::default(),
            estimated_rows: 0,
            failures: 0,
            latency: None,
            latency_samples: 0,
            per_run: None,
            per_request: None,
            models: Vec::new(),
            unpriced: false,
            unpriced_models: Vec::new(),
            outcomes: OutcomeStats::default(),
            span_secs: 0.0,
        }
    }

    fn finish(&mut self) {
        self.per_run = (self.runs > 0).then(|| {
            let n = self.runs as f64;
            PerRunFigures {
                turns: self.turns as f64 / n,
                cost_usd: self.cost_usd / n,
                tokens: self.tokens.total as f64 / n,
                span_secs: self.span_secs / n,
            }
        });
        self.per_request = (self.requests > 0).then(|| {
            let n = self.requests as f64;
            PerRequestFigures {
                cost_usd: self.cost_usd / n,
                tokens_in: self.tokens.prompt as f64 / n,
                tokens_out: self.tokens.completion as f64 / n,
            }
        });
        self.unpriced = !self.unpriced_models.is_empty();
    }
}

/// The whole experiment. Run-level and request-level columns are sums of the
/// per-variant ones, so the two views always agree.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ExperimentTotals {
    pub runs: i64,
    /// Runs seen under more than one variant, each counted once.
    pub mixed_runs: i64,
    pub requests: i64,
    pub unbound_requests: i64,
    pub turns: i64,
    pub cost_usd: f64,
    pub saved_usd: f64,
    pub tokens: TokenTotals,
    pub estimated_rows: i64,
    pub failures: i64,
    pub latency_samples: i64,
    pub outcomes: OutcomeStats,
}

/// The reported outcome of one run, as it appears on the run's row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunOutcomeView {
    pub outcome: String,
    pub score: Option<f64>,
    pub rating: Option<i64>,
    pub note: Option<String>,
    pub reported_at: String,
}

impl From<&RunOutcome> for RunOutcomeView {
    fn from(o: &RunOutcome) -> Self {
        RunOutcomeView {
            outcome: o.outcome.clone(),
            score: o.score,
            rating: o.rating,
            note: o.note.clone(),
            reported_at: o.updated_at.clone(),
        }
    }
}

/// One run: every request sharing `(user_id, correlation_id)`.
///
/// `turns` is how many requests the run took (spec §7a), read from the ledger
/// so it is counted even where prompt rows are not stored; at the run level
/// it therefore equals `requests`. The two diverge only in the per-variant
/// view, where `requests` follows each row's variant and `turns` the run's.
/// A run whose every request failed has no ledger rows and appears with
/// `turns: 0` and its failure count; its span comes from the failure rows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunResult {
    pub user_id: i64,
    pub correlation_id: String,
    /// Variant of the earliest stamped row.
    pub variant: String,
    /// Seen under more than one variant.
    pub mixed: bool,
    pub requests: i64,
    pub unbound_requests: i64,
    pub turns: i64,
    pub cost_usd: f64,
    pub saved_usd: f64,
    pub tokens: TokenTotals,
    pub estimated_rows: i64,
    pub failures: i64,
    pub latency: Option<RunLatency>,
    pub latency_samples: i64,
    /// Seconds between the earliest and latest stamped rows.
    pub span_secs: f64,
    pub first_at: String,
    pub last_at: String,
    pub outcome: Option<RunOutcomeView>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunsPage {
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
    pub items: Vec<RunResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExperimentResults {
    pub experiment: Experiment,
    /// RFC3339; a closed experiment's figures are not frozen (a later price
    /// or purge change can move them), so the document says when it was read.
    pub computed_at: String,
    pub variants: Vec<VariantResults>,
    pub totals: ExperimentTotals,
    /// Stored prompt and response bytes for the experiment; only present
    /// when it retains content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retained_content_bytes: Option<i64>,
    pub runs: RunsPage,
}

type RunKey = (i64, String);

/// What the builder knows about every run before paging: its variant, the
/// figures attributed at run level, and whether it has ledger rows at all.
#[derive(Debug, Clone)]
struct RunIndex {
    variant: String,
    mixed: bool,
    requests: i64,
    first_at: String,
    last_at: String,
    /// False for a run known only from failure rows.
    in_ledger: bool,
}

/// The variant's slot, created empty on first sight.
fn slot<'a>(
    variants: &'a mut BTreeMap<String, VariantResults>,
    label: &str,
) -> &'a mut VariantResults {
    if !variants.contains_key(label) {
        variants.insert(label.to_string(), VariantResults::empty(label));
    }
    variants.get_mut(label).expect("inserted above")
}

fn key_of(user_id: i64, correlation_id: &str) -> RunKey {
    (user_id, correlation_id.to_string())
}

/// Seconds between two RFC3339 timestamps; zero when either fails to parse.
fn span_secs(first_at: &str, last_at: &str) -> f64 {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    match (parse(first_at), parse(last_at)) {
        (Some(a), Some(b)) => (b - a).num_milliseconds().max(0) as f64 / 1000.0,
        _ => 0.0,
    }
}

/// Assemble the results document for experiment `id`.
///
/// Runs are grouped in SQL by user and correlation id over the ledger; prompt
/// rows, failure rows and outcomes are fetched per experiment and joined here
/// by that key, because the prompt log may be a separate database. The run
/// list is ordered by last ledger activity, newest first; runs with no
/// ledger rows (every request failed) follow, newest failure first.
pub async fn build_results(
    sources: &ExperimentSources,
    id: i64,
    page: RunPage,
) -> Result<ExperimentResults, ResultsError> {
    let db = &*sources.db;
    let experiment = ExperimentRepository::get(db, id)
        .await?
        .ok_or_else(|| ResultsError::Invalid(format!("no experiment with id {id}")))?;

    let (variant_totals, variant_models, run_keys, ledger_total, page_rows, unbound) =
        tokio::try_join!(
            CostRepository::experiment_variant_totals(db, id),
            CostRepository::experiment_variant_models(db, id),
            CostRepository::experiment_run_keys(db, id),
            CostRepository::experiment_run_count(db, id),
            CostRepository::experiment_runs(db, id, page.limit, page.offset),
            CostRepository::experiment_unbound_requests(db, id),
        )?;
    let (failures, outcomes, run_latency) = tokio::try_join!(
        FailureRepository::experiment_run_failures(db, id),
        OutcomeRepository::for_experiment(db, id),
        PromptRepository::experiment_run_latency(&*sources.prompt_db, id),
    )?;
    let retained_content_bytes = if experiment.retain_content {
        Some(PromptRepository::experiment_content_bytes(&*sources.prompt_db, id).await?)
    } else {
        None
    };

    // ── Run index: ledger runs, then runs known only from failures ──────────
    let mut index: BTreeMap<RunKey, RunIndex> = run_keys
        .into_iter()
        .map(|k: ExperimentRunKey| {
            (
                key_of(k.user_id, &k.correlation_id),
                RunIndex {
                    variant: k.variant,
                    mixed: k.mixed,
                    requests: k.requests,
                    first_at: k.first_at,
                    last_at: k.last_at,
                    in_ledger: true,
                },
            )
        })
        .collect();

    let mut failures_by_key: HashMap<RunKey, i64> = HashMap::new();
    let mut failures_by_variant: BTreeMap<String, i64> = BTreeMap::new();
    for f in &failures {
        let key = key_of(f.user_id, &f.correlation_id);
        *failures_by_key.entry(key.clone()).or_default() += f.failures;
        *failures_by_variant
            .entry(f.variant.clone().unwrap_or_default())
            .or_default() += f.failures;
        match index.get_mut(&key) {
            Some(run) if run.in_ledger => {}
            Some(run) => {
                // A second variant group of a failure-only run.
                if f.first_at < run.first_at {
                    run.first_at = f.first_at.clone();
                    run.variant = f.variant.clone().unwrap_or_default();
                }
                if f.last_at > run.last_at {
                    run.last_at = f.last_at.clone();
                }
                run.mixed = true;
            }
            None => {
                index.insert(
                    key,
                    RunIndex {
                        variant: f.variant.clone().unwrap_or_default(),
                        mixed: false,
                        requests: 0,
                        first_at: f.first_at.clone(),
                        last_at: f.last_at.clone(),
                        in_ledger: false,
                    },
                );
            }
        }
    }

    let unbound_by_key: HashMap<RunKey, i64> = unbound
        .into_iter()
        .map(|u| (key_of(u.user_id, &u.correlation_id), u.requests))
        .collect();
    let latency_by_key: HashMap<RunKey, RunLatency> = run_latency
        .into_iter()
        .filter(|l| l.samples > 0)
        .map(|l| {
            (
                key_of(l.user_id, &l.correlation_id),
                RunLatency { samples: l.samples, mean_ms: l.mean_ms.unwrap_or(0.0) },
            )
        })
        .collect();
    let outcomes_by_key: HashMap<RunKey, RunOutcome> = outcomes
        .into_iter()
        .map(|o| (key_of(o.user_id, &o.attribution_correlation_id), o))
        .collect();

    // ── Per-variant aggregates ──────────────────────────────────────────────
    // Declared labels first so a variant with no traffic still appears; any
    // label found on a row but not declared (a variant that no longer exists
    // is impossible today, but the rows are the record) is added after.
    let mut variants: BTreeMap<String, VariantResults> = experiment
        .variants
        .keys()
        .map(|label| (label.clone(), VariantResults::empty(label)))
        .collect();

    for t in &variant_totals {
        let v = slot(&mut variants, &t.variant);
        v.requests += t.requests;
        v.cost_usd += t.cost_usd;
        v.saved_usd += t.saved_usd;
        v.tokens.add(TokenTotals::new(t.tokens_in, t.tokens_out));
        v.estimated_rows += t.estimated_rows;
    }
    for m in &variant_models {
        let unpriced = !sources.cost_calc.has_price(&m.model);
        let v = slot(&mut variants, &m.variant);
        if unpriced && !v.unpriced_models.contains(&m.model) {
            v.unpriced_models.push(m.model.clone());
        }
        v.models.push(VariantModelMetrics {
            model: m.model.clone(),
            requests: m.requests,
            cost_usd: m.cost_usd,
            saved_usd: m.saved_usd,
            tokens: TokenTotals::new(m.tokens_in, m.tokens_out),
            estimated_rows: m.estimated_rows,
            unpriced,
        });
    }
    for (label, n) in &failures_by_variant {
        slot(&mut variants, label).failures += n;
    }
    for (key, run) in &index {
        let v = slot(&mut variants, &run.variant);
        v.runs += 1;
        v.mixed_runs += i64::from(run.mixed);
        v.turns += run.requests;
        v.unbound_requests += unbound_by_key.get(key).copied().unwrap_or(0);
        v.span_secs += span_secs(&run.first_at, &run.last_at);
    }
    for (key, o) in &outcomes_by_key {
        // The run's variant from the ledger wins; the stamp on the outcome is
        // the same rule applied at feedback time and covers a run whose rows
        // have since been purged.
        let label = index
            .get(key)
            .map(|r| r.variant.clone())
            .or_else(|| o.experiment_variant.clone())
            .unwrap_or_default();
        slot(&mut variants, &label).outcomes.add(o);
    }
    for label in variants.keys().cloned().collect::<Vec<_>>() {
        let filter = ArmFilter::Variant { experiment_id: id, variant: label.clone() };
        let summary = PromptRepository::latency_summary(
            &*sources.prompt_db,
            &filter,
            ALL_TIME_START,
            ALL_TIME_END,
        )
        .await?;
        let v = slot(&mut variants, &label);
        v.latency_samples = summary.samples;
        v.latency = (summary.samples > 0).then_some(summary);
        v.finish();
    }

    // ── Totals ──────────────────────────────────────────────────────────────
    let mut totals = ExperimentTotals::default();
    for v in variants.values() {
        totals.runs += v.runs;
        totals.mixed_runs += v.mixed_runs;
        totals.requests += v.requests;
        totals.unbound_requests += v.unbound_requests;
        totals.turns += v.turns;
        totals.cost_usd += v.cost_usd;
        totals.saved_usd += v.saved_usd;
        totals.tokens.add(v.tokens);
        totals.estimated_rows += v.estimated_rows;
        totals.failures += v.failures;
        totals.latency_samples += v.latency_samples;
    }
    for o in outcomes_by_key.values() {
        totals.outcomes.add(o);
    }

    // ── Run page ────────────────────────────────────────────────────────────
    let run_result = |key: &RunKey, run: &RunIndex, row: Option<&ExperimentRunRow>| RunResult {
        user_id: key.0,
        correlation_id: key.1.clone(),
        variant: run.variant.clone(),
        mixed: run.mixed,
        requests: run.requests,
        unbound_requests: unbound_by_key.get(key).copied().unwrap_or(0),
        turns: run.requests,
        cost_usd: row.map(|r| r.cost_usd).unwrap_or(0.0),
        saved_usd: row.map(|r| r.saved_usd).unwrap_or(0.0),
        tokens: row
            .map(|r| TokenTotals::new(r.tokens_in, r.tokens_out))
            .unwrap_or_default(),
        estimated_rows: row.map(|r| r.estimated_rows).unwrap_or(0),
        failures: failures_by_key.get(key).copied().unwrap_or(0),
        latency: latency_by_key.get(key).copied(),
        latency_samples: latency_by_key.get(key).map(|l| l.samples).unwrap_or(0),
        span_secs: span_secs(&run.first_at, &run.last_at),
        first_at: run.first_at.clone(),
        last_at: run.last_at.clone(),
        outcome: outcomes_by_key.get(key).map(RunOutcomeView::from),
    };

    let mut items: Vec<RunResult> = page_rows
        .iter()
        .map(|row| {
            let key = key_of(row.user_id, &row.correlation_id);
            // The page and the index are separate reads; a run that landed
            // between them is described from its page row alone.
            let fallback = RunIndex {
                variant: row.variant.clone(),
                mixed: row.mixed(),
                requests: row.requests,
                first_at: row.first_at.clone(),
                last_at: row.last_at.clone(),
                in_ledger: true,
            };
            let run = index.get(&key).unwrap_or(&fallback);
            run_result(&key, run, Some(row))
        })
        .collect();

    let mut failure_only: Vec<(&RunKey, &RunIndex)> =
        index.iter().filter(|(_, r)| !r.in_ledger).collect();
    failure_only.sort_by(|a, b| b.1.last_at.cmp(&a.1.last_at).then_with(|| a.0.cmp(b.0)));
    let total = ledger_total + failure_only.len() as i64;
    let room = (page.limit - items.len() as i64).max(0) as usize;
    if room > 0 {
        let skip = (page.offset - ledger_total).max(0) as usize;
        items.extend(
            failure_only
                .into_iter()
                .skip(skip)
                .take(room)
                .map(|(key, run)| run_result(key, run, None)),
        );
    }

    Ok(ExperimentResults {
        experiment,
        computed_at: chrono::Utc::now().to_rfc3339(),
        variants: variants.into_values().collect(),
        totals,
        retained_content_bytes,
        runs: RunsPage { total, limit: page.limit, offset: page.offset, items },
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct ResultsQuery {
    #[serde(default)]
    pub limit: Option<String>,
    #[serde(default)]
    pub offset: Option<String>,
}

/// GET /admin/api/experiments/:id/results?limit=&offset=
pub async fn get_experiment_results_api(
    State(state): State<AppState>,
    _session: AdminSession,
    Path(id): Path<String>,
    Query(q): Query<ResultsQuery>,
) -> Result<Json<ExperimentResults>, ApiError> {
    let id = parse_id(&id)?;
    let page = RunPage::parse(q.limit.as_deref(), q.offset.as_deref())
        .map_err(ApiError::InvalidRequest)?;
    let sources = ExperimentSources::from_state(&state);
    Ok(Json(build_results(&sources, id, page).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    fn good_body() -> Value {
        json!({
            "name": "exp",
            "variants": {
                "control": { "fast": "openai/gpt-4o-mini" },
                "candidate": { "fast": "anthropic/claude-haiku-4-5" }
            },
            "expires_at": 0,
            "content_retention_days": 0,
            "retain_content": false
        })
    }

    #[test]
    fn labels_follow_the_header_charset() {
        assert!(is_valid_label("control"));
        assert!(is_valid_label("v1.2_b-3"));
        assert!(!is_valid_label(""));
        assert!(!is_valid_label("has space"));
        assert!(!is_valid_label("colon:no"));
        assert!(!is_valid_label(&"x".repeat(MAX_LABEL_LEN + 1)));
    }

    #[test]
    fn a_complete_body_parses() {
        let p = parse_create(&good_body(), now()).unwrap();
        assert_eq!(p.name, "exp");
        assert_eq!(p.variants.len(), 2);
        assert_eq!(p.variants["control"]["fast"], "openai/gpt-4o-mini");
        assert_eq!(p.expires_at, 0);
        assert!(!p.feed_learning);
        assert!(p.allowed_user_ids.is_empty());
    }

    #[test]
    fn missing_fields_are_named() {
        for field in ["name", "variants", "expires_at", "content_retention_days", "retain_content"] {
            let mut b = good_body();
            b.as_object_mut().unwrap().remove(field);
            let err = parse_create(&b, now()).unwrap_err();
            assert!(err.starts_with(field), "{field}: {err}");
        }
    }

    #[test]
    fn expires_at_accepts_rfc3339_or_zero_only() {
        let mut b = good_body();
        b["expires_at"] = json!("2999-01-01T00:00:00Z");
        assert!(parse_create(&b, now()).unwrap().expires_at > 0);
        b["expires_at"] = json!(1);
        assert!(parse_create(&b, now()).unwrap_err().starts_with("expires_at"));
        b["expires_at"] = json!("tomorrow");
        assert!(parse_create(&b, now()).unwrap_err().starts_with("expires_at"));
        b["expires_at"] = json!("2000-01-01T00:00:00Z");
        assert!(parse_create(&b, now()).unwrap_err().contains("future"));
    }

    #[test]
    fn retain_content_needs_an_expiry() {
        let mut b = good_body();
        b["retain_content"] = json!(true);
        let err = parse_create(&b, now()).unwrap_err();
        assert!(err.contains("retain_content") && err.contains("expires_at"), "{err}");
    }

    #[test]
    fn variant_bounds_are_enforced() {
        let mut b = good_body();
        b["variants"] = json!({ "only": { "a": "openai/gpt-4o" } });
        assert!(parse_create(&b, now()).unwrap_err().starts_with("variants"));

        let mut many = serde_json::Map::new();
        for i in 0..(MAX_VARIANTS + 1) {
            many.insert(format!("v{i}"), json!({ "a": "openai/gpt-4o" }));
        }
        b["variants"] = Value::Object(many);
        assert!(parse_create(&b, now()).unwrap_err().starts_with("variants"));

        b["variants"] = json!({ "bad label": { "a": "openai/gpt-4o" }, "ok": {} });
        assert!(parse_create(&b, now()).unwrap_err().contains("bad label"));

        let mut wide = serde_json::Map::new();
        for i in 0..(MAX_OVERLAY_ENTRIES + 1) {
            wide.insert(format!("m{i}"), json!("openai/gpt-4o"));
        }
        b["variants"] = json!({ "a": Value::Object(wide), "b": {} });
        assert!(parse_create(&b, now()).unwrap_err().contains("at most"));
    }

    #[test]
    fn retention_days_are_bounded() {
        let mut b = good_body();
        b["content_retention_days"] = json!(MAX_RETENTION_DAYS + 1);
        assert!(parse_create(&b, now()).unwrap_err().starts_with("content_retention_days"));
        b["content_retention_days"] = json!(-1);
        assert!(parse_create(&b, now()).unwrap_err().starts_with("content_retention_days"));
        b["content_retention_days"] = json!("30");
        assert!(parse_create(&b, now()).unwrap_err().starts_with("content_retention_days"));
    }

    #[test]
    fn run_page_defaults_bounds_and_names_the_field() {
        assert_eq!(RunPage::parse(None, None).unwrap(), RunPage::default());
        assert_eq!(RunPage::parse(Some(""), Some(" ")).unwrap(), RunPage::default());
        assert_eq!(
            RunPage::parse(Some("1"), Some("1")).unwrap(),
            RunPage { limit: 1, offset: 1 }
        );
        assert_eq!(RunPage::parse(Some("1000"), None).unwrap().limit, MAX_RUN_LIMIT);
        for bad in ["0", "1001", "-1", "ten", "1.5"] {
            assert!(RunPage::parse(Some(bad), None).unwrap_err().starts_with("limit"), "{bad}");
        }
        for bad in ["-1", "x", "0.5"] {
            assert!(RunPage::parse(None, Some(bad)).unwrap_err().starts_with("offset"), "{bad}");
        }
    }

    #[test]
    fn span_is_seconds_between_timestamps_and_zero_when_unparseable() {
        assert_eq!(span_secs("2026-01-01T00:00:00Z", "2026-01-01T00:01:30Z"), 90.0);
        assert_eq!(span_secs("2026-01-01T00:00:00+00:00", "2026-01-01T00:00:00.500Z"), 0.5);
        assert_eq!(span_secs("2026-01-01T00:01:00Z", "2026-01-01T00:00:00Z"), 0.0);
        assert_eq!(span_secs("nope", "2026-01-01T00:00:00Z"), 0.0);
    }

    #[test]
    fn outcome_stats_average_only_what_was_reported() {
        let outcome = |outcome: &str, score: Option<f64>, rating: Option<i64>| RunOutcome {
            user_id: 1,
            attribution_correlation_id: "r".into(),
            outcome: outcome.into(),
            score,
            rating,
            note: None,
            experiment_id: None,
            experiment_variant: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let mut stats = OutcomeStats::default();
        assert_eq!(stats.success_rate, None);
        stats.add(&outcome("success", Some(1.0), Some(5)));
        stats.add(&outcome("failure", None, Some(2)));
        stats.add(&outcome("success", Some(0.5), None));
        assert_eq!(stats.reported, 3);
        assert_eq!(stats.success, 2);
        assert_eq!(stats.failure, 1);
        assert!((stats.success_rate.unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(stats.mean_score, Some(0.75));
        assert_eq!(stats.score_samples, 2);
        assert_eq!(stats.mean_rating, Some(3.5));
        assert_eq!(stats.rating_samples, 2);
    }

    #[test]
    fn audit_row_spells_out_never() {
        let exp = Experiment {
            id: 1,
            name: "e".into(),
            variants: BTreeMap::new(),
            allowed_user_ids: vec![],
            status: crate::db::models::ExperimentStatus::Active,
            feed_learning: false,
            expires_at: 0,
            created_at: "2026-01-01T00:00:00+00:00".into(),
            closed_at: None,
            retain_content: false,
            content_retention_days: 0,
        };
        let v = audit_row(&exp);
        assert_eq!(v["expires_at"], "never");
        assert_eq!(v["content_retention_days"], "never");
        let dated = Experiment { expires_at: 5, content_retention_days: 30, ..exp };
        let v = audit_row(&dated);
        assert_eq!(v["expires_at"], 5);
        assert_eq!(v["content_retention_days"], 30);
    }
}
