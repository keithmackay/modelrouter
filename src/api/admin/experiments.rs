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

use std::collections::BTreeMap;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::audit::audit;
use crate::api::{
    admin::auth::{AdminSession, SuperAdminSession},
    app::AppState,
    error::ApiError,
};
use crate::db::models::{Experiment, ExperimentVariants, NewExperiment, VariantTarget};
use crate::db::repositories::experiments::{ExperimentRepository, ExperimentStatusFilter};
use crate::db::repositories::users::UserRepository;
use crate::router::experiments::MAX_LABEL_LEN;

/// Bounds on a create request. Labels are further limited to
/// [`MAX_LABEL_LEN`] so they always fit the request header.
const MAX_NAME_LEN: usize = 128;
const MIN_VARIANTS: usize = 2;
const MAX_VARIANTS: usize = 16;
const MAX_OVERLAY_ENTRIES: usize = 32;
const MAX_EXPR_LEN: usize = 128;
const MAX_RETENTION_DAYS: i64 = 3650;

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

/// Whether `label` is a legal variant label: `[A-Za-z0-9_.-]{1,64}`.
fn label_ok(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_LEN
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
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
        if !label_ok(label) {
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

// ── Handlers ──────────────────────────────────────────────────────────────────

/// POST /admin/api/experiments
pub async fn create_experiment_api(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let now = chrono::Utc::now();
    let parsed = parse_create(&body, now).map_err(ApiError::InvalidRequest)?;

    let existing = ExperimentRepository::list(&*state.db, ExperimentStatusFilter::All)
        .await
        .map_err(|_| ApiError::Internal)?;
    if existing.iter().any(|e| e.name == parsed.name) {
        return Err(ApiError::InvalidRequest(format!(
            "name '{}' is already taken",
            parsed.name
        )));
    }

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

    let row = ExperimentRepository::create(
        &*state.db,
        NewExperiment {
            name: parsed.name,
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
        assert!(label_ok("control"));
        assert!(label_ok("v1.2_b-3"));
        assert!(!label_ok(""));
        assert!(!label_ok("has space"));
        assert!(!label_ok("colon:no"));
        assert!(!label_ok(&"x".repeat(MAX_LABEL_LEN + 1)));
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
