//! Admin read of `usage` events by ledger id.
//!
//! `GET /admin/api/usage/events` returns the same payloads the `usage` webhook
//! delivers, built by the same serializer from the stored rows, so a receiver
//! that missed deliveries can page through the ledger and fill the gaps. Items
//! carry no `delivery_id`: that names one webhook delivery, and a read is not
//! one. Receivers dedupe on `ledger_id`.

use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{app::AppState, error::ApiError};
use crate::callbacks::usage::from_ledger;
use crate::db::repositories::costs::CostRepository;

use super::auth::AdminSession;

/// Most events one page holds, and the default page size.
pub const MAX_LIMIT: i64 = 1000;

/// Raw query parameters, parsed by [`UsageEventsQuery::page`] so a bad value
/// is refused with a message naming its field.
#[derive(Debug, Default, Deserialize)]
pub struct UsageEventsQuery {
    pub since: Option<String>,
    pub after_id: Option<String>,
    pub limit: Option<String>,
}

/// One validated page request.
#[derive(Debug, PartialEq)]
pub struct Page {
    /// `since` as RFC3339 UTC, the form the ledger stores, so the store can
    /// compare it to `created_at` as text.
    pub since: Option<String>,
    pub after_id: i64,
    pub limit: i64,
}

impl UsageEventsQuery {
    pub fn page(&self) -> Result<Page, ApiError> {
        let since = match &self.since {
            None => None,
            Some(s) => Some(
                chrono::DateTime::parse_from_rfc3339(s)
                    .map_err(|_| ApiError::InvalidRequest("since must be an RFC 3339 timestamp".into()))?
                    .with_timezone(&chrono::Utc)
                    .to_rfc3339(),
            ),
        };
        let after_id = match &self.after_id {
            None => 0,
            Some(s) => s
                .parse()
                .map_err(|_| ApiError::InvalidRequest("after_id must be an integer".into()))?,
        };
        let limit = match &self.limit {
            None => MAX_LIMIT,
            Some(s) => s
                .parse()
                .ok()
                .filter(|n| (1..=MAX_LIMIT).contains(n))
                .ok_or_else(|| {
                    ApiError::InvalidRequest(format!("limit must be an integer from 1 to {MAX_LIMIT}"))
                })?,
        };
        Ok(Page { since, after_id, limit })
    }
}

/// GET /admin/api/usage/events?since=&after_id=&limit=
///
/// `next_after_id` is the last event's ledger id when the page is full (more
/// may follow), and null when it is not (the reader has caught up).
pub async fn get_usage_events(
    State(state): State<AppState>,
    _session: AdminSession,
    Query(q): Query<UsageEventsQuery>,
) -> Result<axum::Json<Value>, ApiError> {
    let page = q.page()?;
    let rows = CostRepository::list_usage_after(
        &*state.db,
        page.since.as_deref(),
        page.after_id,
        page.limit,
    )
    .await
    .map_err(|_| ApiError::Internal)?;
    let next_after_id = match rows.last() {
        Some(last) if rows.len() as i64 == page.limit => Some(last.id),
        _ => None,
    };
    let events: Vec<Value> = rows.iter().map(from_ledger).collect();
    Ok(axum::Json(json!({ "events": events, "next_after_id": next_after_id })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(since: Option<&str>, after_id: Option<&str>, limit: Option<&str>) -> UsageEventsQuery {
        UsageEventsQuery {
            since: since.map(str::to_string),
            after_id: after_id.map(str::to_string),
            limit: limit.map(str::to_string),
        }
    }

    #[test]
    fn no_parameters_read_from_the_start_a_full_page_at_a_time() {
        assert_eq!(
            q(None, None, None).page().unwrap(),
            Page { since: None, after_id: 0, limit: MAX_LIMIT }
        );
    }

    #[test]
    fn since_is_held_in_utc() {
        let page = q(Some("2026-11-14T17:02:11.5+02:00"), Some("41"), Some("7")).page().unwrap();
        assert_eq!(
            page,
            Page { since: Some("2026-11-14T15:02:11.500+00:00".into()), after_id: 41, limit: 7 }
        );
    }
}
