//! `POST /v1/feedback`: a caller reports how a run turned out.
//!
//! The router sees every request in a run but never whether the run as a whole
//! succeeded — that is known only to the application that drove it. This
//! endpoint closes the loop: one outcome per `(user, correlation_id)`, written
//! under the caller's API key, replaced by a later report for the same run.
//!
//! The outcome is stamped with the experiment id and variant of the run's
//! earliest stamped ledger row, so results can be grouped by variant without a
//! second lookup. Nothing here reads or rejects the experiment header; a report
//! is about a run that already happened.

use axum::{extract::State, Json};
use serde_json::Value;

use crate::{
    api::{app::AppState, attribution::MAX_CORRELATION_LEN, auth::AuthenticatedUser, error::ApiError},
    db::{
        models::{NewRunOutcome, RunOutcome},
        repositories::{costs::CostRepository, failures::FailureRepository, outcomes::OutcomeRepository},
    },
};

/// Values `outcome` may take. Anything else is a 400: a free-text outcome
/// cannot be counted, and a misspelling would silently form its own bucket.
pub const OUTCOMES: &[&str] = &["success", "failure"];
/// Upper bound on `note`, in characters. Notes are bounded metadata (a reason
/// code, a ticket id), never prompt or response content.
pub const MAX_NOTE_LEN: usize = 1024;

/// Record the outcome of a run under the caller's key. Last write wins.
///
/// Body fields:
/// - `correlation_id` (required): the run's attribution correlation id, 1 to
///   128 printable-ASCII characters, as sent on its requests.
/// - `outcome` (required): `success` or `failure`.
/// - `score` (optional): number in `[0, 1]`.
/// - `rating` (optional): integer 1 to 5.
/// - `note` (optional): string of at most 1024 characters.
///
/// The run must already have a ledger or failure row under the caller's user
/// for this correlation id. Cost logging is asynchronous, so a report sent in
/// the same instant as the run's last response can race it; the 400 says so
/// and the caller may retry.
///
/// Returns 200 with the stored `run_outcomes` row.
pub async fn post_feedback(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Json<RunOutcome>, ApiError> {
    let user = user.0;
    let obj = body
        .as_object()
        .ok_or_else(|| ApiError::InvalidRequest("body must be a JSON object".to_string()))?;

    let correlation_id = match obj.get("correlation_id") {
        None | Some(Value::Null) => {
            return Err(ApiError::InvalidRequest(
                "correlation_id is required".to_string(),
            ))
        }
        Some(Value::String(s)) => validate_correlation_id(s)?,
        Some(_) => {
            return Err(ApiError::InvalidRequest(
                "correlation_id must be a string".to_string(),
            ))
        }
    };

    let outcome = match obj.get("outcome") {
        None | Some(Value::Null) => {
            return Err(ApiError::InvalidRequest("outcome is required".to_string()))
        }
        Some(Value::String(s)) if OUTCOMES.contains(&s.as_str()) => s.clone(),
        Some(_) => {
            return Err(ApiError::InvalidRequest(format!(
                "outcome must be one of: {}",
                OUTCOMES.join(", ")
            )))
        }
    };

    let score = match obj.get("score") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_f64() {
            Some(f) if (0.0..=1.0).contains(&f) => Some(f),
            _ => {
                return Err(ApiError::InvalidRequest(
                    "score must be a number between 0 and 1".to_string(),
                ))
            }
        },
    };

    let rating = match obj.get("rating") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_i64() {
            Some(n) if (1..=5).contains(&n) => Some(n),
            _ => {
                return Err(ApiError::InvalidRequest(
                    "rating must be an integer between 1 and 5".to_string(),
                ))
            }
        },
    };

    let note = match obj.get("note") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.chars().count() <= MAX_NOTE_LEN => Some(s.clone()),
        Some(Value::String(_)) => {
            return Err(ApiError::InvalidRequest(format!(
                "note must be at most {} characters",
                MAX_NOTE_LEN
            )))
        }
        Some(_) => {
            return Err(ApiError::InvalidRequest(
                "note must be a string".to_string(),
            ))
        }
    };

    // The run must be the caller's: a correlation id is caller-chosen, so the
    // same string under another key is a different run, and a report for it
    // must neither land on nor reveal that run. A failure-only run (every
    // request failed, so no ledger row) still counts as having happened.
    let stamp = CostRepository::run_stamp(&*state.db, user.id, &correlation_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to look up run stamp for feedback");
            ApiError::Internal
        })?;
    let stamp = match stamp {
        Some(stamp) => stamp,
        None => {
            let failed = FailureRepository::has_rows_for_user(&*state.db, user.id, &correlation_id)
                .await
                .map_err(|e| {
                    tracing::error!(error = %e, "Failed to look up failures for feedback");
                    ApiError::Internal
                })?;
            if !failed {
                return Err(ApiError::InvalidRequest(format!(
                    "correlation_id '{}' has no recorded requests under this API key yet; \
                     requests are logged asynchronously, so retry shortly if the run just finished",
                    correlation_id
                )));
            }
            Default::default()
        }
    };

    let row = OutcomeRepository::upsert(
        &*state.db,
        NewRunOutcome {
            user_id: user.id,
            attribution_correlation_id: correlation_id,
            outcome,
            score,
            rating,
            note,
            experiment_id: stamp.experiment_id,
            experiment_variant: stamp.experiment_variant,
        },
    )
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "Failed to record run outcome");
        ApiError::Internal
    })?;

    Ok(Json(row))
}

/// Same shape rules as `attribution.correlation_id`, so an id accepted on a
/// request is accepted here and vice versa: non-empty after trimming, bounded
/// length, printable ASCII.
fn validate_correlation_id(raw: &str) -> Result<String, ApiError> {
    let v = raw.trim();
    if v.is_empty() {
        return Err(ApiError::InvalidRequest(
            "correlation_id must not be empty".to_string(),
        ));
    }
    if v.chars().count() > MAX_CORRELATION_LEN {
        return Err(ApiError::InvalidRequest(format!(
            "correlation_id must be at most {} characters",
            MAX_CORRELATION_LEN
        )));
    }
    if !v.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return Err(ApiError::InvalidRequest(
            "correlation_id contains unsupported characters".to_string(),
        ));
    }
    Ok(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correlation_id_is_trimmed_and_bounded() {
        assert_eq!(validate_correlation_id("  run-1 ").unwrap(), "run-1");
        assert!(validate_correlation_id("   ").is_err());
        assert!(validate_correlation_id(&"x".repeat(MAX_CORRELATION_LEN)).is_ok());
        assert!(validate_correlation_id(&"x".repeat(MAX_CORRELATION_LEN + 1)).is_err());
        assert!(validate_correlation_id("a\nb").is_err());
    }
}
