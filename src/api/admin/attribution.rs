//! Admin surface for attribution-filtered usage (issue #13).
//!
//! Answers "what did *this* unit of work cost, and what did the cache save on
//! it" for a caller-supplied correlation id or tag. Exposed as JWT-gated REST
//! under `/admin/api/usage/attribution*` and, for operators, as an extra filter
//! on the existing Reports page — the reports page owns the chrome, this module
//! only supplies the panel body.

use axum::extract::{Query, State};
use serde::Deserialize;

use crate::api::{app::AppState, error::ApiError};
use crate::db::repositories::costs::{AttributionFilter, AttributionTotals, CostRepository};

use super::auth::AdminSession;

/// Cap on how many distinct values the pickers will load.
pub(super) const FACET_LIMIT: i64 = 500;

/// Query shared by the REST endpoint and the dashboard panel.
///
/// `key` empty ⇒ filter on the correlation id; otherwise on tag `key`.
#[derive(Debug, Default, Deserialize)]
pub struct AttributionQuery {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub value: String,
    #[serde(default = "default_window")]
    pub window: String,
}

pub(super) fn default_window() -> String {
    "monthly".to_string()
}

impl AttributionQuery {
    /// The filter this query selects, or `None` when no value was supplied.
    pub fn filter(&self) -> Result<Option<AttributionFilter>, ApiError> {
        let value = self.value.trim();
        if value.is_empty() {
            return Ok(None);
        }
        let key = self.key.trim();
        if key.is_empty() {
            return Ok(Some(AttributionFilter::CorrelationId(value.to_string())));
        }
        if !crate::api::attribution::is_safe_tag_key(key) {
            return Err(ApiError::InvalidRequest(
                "attribution tag key must contain only letters, digits, '_', '-', '.' or ':'"
                    .to_string(),
            ));
        }
        Ok(Some(AttributionFilter::Tag {
            key: key.to_string(),
            value: value.to_string(),
        }))
    }
}

/// The full report for one attribution value.
#[derive(Debug, serde::Serialize)]
pub struct AttributionReport {
    pub filter: String,
    pub window: String,
    pub start: String,
    pub end: String,
    pub totals: AttributionTotals,
    pub hit_rate: f64,
    pub by_model: Vec<crate::db::repositories::costs::AttributionBreakdownRow>,
    pub by_day: Vec<crate::db::repositories::costs::AttributionBreakdownRow>,
}

/// Build the report. Shared by the REST endpoint, the dashboard panel and the
/// CLI, so all three necessarily agree.
pub async fn build_report(
    state: &AppState,
    filter: &AttributionFilter,
    window: &str,
) -> anyhow::Result<AttributionReport> {
    let (start, end) = window_range(window);
    let totals = CostRepository::attribution_totals(&*state.db, filter, &start, &end).await?;
    let by_model = CostRepository::attribution_by_model(&*state.db, filter, &start, &end).await?;
    let by_day = CostRepository::attribution_by_day(&*state.db, filter, &start, &end).await?;
    Ok(AttributionReport {
        filter: filter.label(),
        window: window.to_string(),
        start,
        end,
        hit_rate: totals.hit_rate(),
        totals,
        by_model,
        by_day,
    })
}

/// `(start, end)` as ISO 8601 UTC for a named window. `all` covers everything.
pub fn window_range(window: &str) -> (String, String) {
    use chrono::{Datelike, Duration, TimeZone, Utc};
    let now = Utc::now();
    let end = (now + Duration::days(1))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let start = match window {
        "all" => "1970-01-01T00:00:00Z".to_string(),
        "daily" => Utc
            .from_utc_datetime(&now.date_naive().and_hms_opt(0, 0, 0).unwrap())
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
        "weekly" => (now - Duration::days(7))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
        _ => {
            let d = chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap();
            Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        }
    };
    (start, end)
}

// ── REST API ──────────────────────────────────────────────────────────────────

/// GET /admin/api/usage/attribution?key=&value=&window=
pub async fn get_attribution_usage(
    State(state): State<AppState>,
    _session: AdminSession,
    Query(q): Query<AttributionQuery>,
) -> Result<axum::Json<AttributionReport>, ApiError> {
    let filter = q.filter()?.ok_or_else(|| {
        ApiError::InvalidRequest("value is required".to_string())
    })?;
    let report = build_report(&state, &filter, &q.window)
        .await
        .map_err(|_| ApiError::Internal)?;
    Ok(axum::Json(report))
}

#[derive(Debug, Default, Deserialize)]
pub struct FacetQuery {
    #[serde(default)]
    pub key: String,
}

/// GET /admin/api/usage/attribution/facets?key=
///
/// With no `key`: the tag keys in the ledger plus the known correlation ids.
/// With a `key`: the distinct values recorded for that tag.
pub async fn get_attribution_facets(
    State(state): State<AppState>,
    _session: AdminSession,
    Query(q): Query<FacetQuery>,
) -> Result<axum::Json<serde_json::Value>, ApiError> {
    let key = q.key.trim();
    if key.is_empty() {
        let keys = CostRepository::distinct_attribution_tag_keys(&*state.db)
            .await
            .map_err(|_| ApiError::Internal)?;
        let correlation_ids =
            CostRepository::distinct_attribution_values(&*state.db, None, FACET_LIMIT)
                .await
                .map_err(|_| ApiError::Internal)?;
        return Ok(axum::Json(serde_json::json!({
            "tag_keys": keys,
            "correlation_ids": correlation_ids,
        })));
    }
    if !crate::api::attribution::is_safe_tag_key(key) {
        return Err(ApiError::InvalidRequest(
            "attribution tag key must contain only letters, digits, '_', '-', '.' or ':'"
                .to_string(),
        ));
    }
    let values = CostRepository::distinct_attribution_values(&*state.db, Some(key), FACET_LIMIT)
        .await
        .map_err(|_| ApiError::Internal)?;
    Ok(axum::Json(serde_json::json!({ "key": key, "values": values })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(key: &str, value: &str) -> AttributionQuery {
        AttributionQuery {
            key: key.to_string(),
            value: value.to_string(),
            window: "monthly".to_string(),
        }
    }

    #[test]
    fn empty_value_selects_no_filter() {
        assert_eq!(q("", "").filter().unwrap(), None);
        assert_eq!(q("engagement", "  ").filter().unwrap(), None);
    }

    #[test]
    fn no_key_means_correlation_id() {
        assert_eq!(
            q("", "run-7").filter().unwrap(),
            Some(AttributionFilter::CorrelationId("run-7".to_string()))
        );
    }

    #[test]
    fn key_and_value_means_tag() {
        assert_eq!(
            q("engagement", "eng-1").filter().unwrap(),
            Some(AttributionFilter::Tag {
                key: "engagement".to_string(),
                value: "eng-1".to_string()
            })
        );
    }

    #[test]
    fn rejects_unsafe_tag_key() {
        assert!(q("bad\"key", "v").filter().is_err());
    }

    #[test]
    fn all_window_starts_at_the_epoch() {
        let (start, _) = window_range("all");
        assert_eq!(start, "1970-01-01T00:00:00Z");
    }
}
