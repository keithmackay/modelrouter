//! Repeated requests: ledger rows that share one request fingerprint.
//!
//! Every ledger row carries the fingerprint of the request it records (the
//! normalised-request hash the response cache keys on, computed for every
//! call). Rows with one fingerprint are one request made more than once, so
//! this report says where work repeated in a window, how much of it the cache
//! absorbed, and which correlation ids made it. It never needs any request
//! content: the fingerprint is a hash.
//!
//! One query serves `GET /admin/api/usage/duplicates` and
//! `modelrouter report duplicates`.

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::auth::AdminSession;
use super::compare::WINDOWS;
use crate::api::app::{AppState, DatabaseProvider};
use crate::api::error::ApiError;
use crate::db::repositories::costs::{CostRepository, DuplicateFilter, DuplicateGroup};
use crate::db::repositories::users::UserRepository;

/// Groups returned when the caller sets no `limit`.
pub const DEFAULT_LIMIT: i64 = 50;
/// The most groups one call returns.
pub const MAX_LIMIT: i64 = 1000;
/// Longest filter value accepted; wider than every ingest bound in
/// `api::attribution` (correlation ids and tag values stop at 128).
const MAX_VALUE_LEN: usize = 256;

/// The query string of `GET /admin/api/usage/duplicates`, and what the CLI
/// builds from its flags. Everything but the window is optional; set filters
/// compose with AND.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DuplicatesQuery {
    /// `all`, `daily`, `weekly` or `monthly` (the default).
    #[serde(default = "super::attribution::default_window")]
    pub window: String,
    /// Only rows whose correlation id starts with this.
    #[serde(default)]
    pub correlation_id_prefix: Option<String>,
    /// Only rows carrying this attribution tag; `tag_value` is required with it.
    #[serde(default)]
    pub tag_key: Option<String>,
    #[serde(default)]
    pub tag_value: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    /// A user name; resolved to the user's rows.
    #[serde(default)]
    pub user: Option<String>,
    /// Groups to return, 1 to [`MAX_LIMIT`]; [`DEFAULT_LIMIT`] when unset.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// The answer: the window as resolved, the filter as applied, the groups and
/// their totals.
#[derive(Debug, Clone, Serialize)]
pub struct DuplicatesReport {
    pub window: String,
    pub start: String,
    pub end: String,
    pub filter: DuplicateFilter,
    pub limit: i64,
    pub totals: DuplicateTotals,
    /// Most repeated first; see `CostRepository::duplicate_requests`.
    pub groups: Vec<DuplicateGroup>,
}

/// Sums over the returned groups.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DuplicateTotals {
    pub groups: i64,
    /// Every row in a returned group, the first occurrence included.
    pub requests: i64,
    /// Rows beyond the first of each group: the calls that repeated one.
    pub repeats: i64,
    pub cache_hits: i64,
    pub cost_usd: f64,
    pub saved_usd: f64,
}

/// Why a query was refused: a 400 on the API, the CLI's error line.
#[derive(Debug, thiserror::Error)]
pub enum DuplicatesError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Db(#[from] anyhow::Error),
}

impl From<DuplicatesError> for ApiError {
    fn from(e: DuplicatesError) -> Self {
        match e {
            DuplicatesError::Invalid(msg) => ApiError::InvalidRequest(msg),
            DuplicatesError::Db(e) => {
                tracing::error!(error = %e, "duplicates report failed");
                ApiError::Internal
            }
        }
    }
}

fn trimmed(value: &Option<String>, name: &str) -> Result<Option<String>, DuplicatesError> {
    let Some(v) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if v.chars().count() > MAX_VALUE_LEN {
        return Err(DuplicatesError::Invalid(format!(
            "{name} is longer than {MAX_VALUE_LEN} characters"
        )));
    }
    Ok(Some(v.to_string()))
}

/// Check the query and resolve the user name, without touching the ledger.
pub async fn resolve(
    db: &dyn DatabaseProvider,
    q: &DuplicatesQuery,
) -> Result<(DuplicateFilter, i64), DuplicatesError> {
    if !WINDOWS.contains(&q.window.as_str()) {
        return Err(DuplicatesError::Invalid(format!(
            "window must be one of {}",
            WINDOWS.join(", ")
        )));
    }
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(DuplicatesError::Invalid(format!(
            "limit must be between 1 and {MAX_LIMIT}"
        )));
    }
    let tag = match (trimmed(&q.tag_key, "tag_key")?, trimmed(&q.tag_value, "tag_value")?) {
        (None, None) => None,
        (Some(key), Some(value)) => {
            if !crate::api::attribution::is_safe_tag_key(&key) {
                return Err(DuplicatesError::Invalid(
                    "tag_key must contain only letters, digits, '_', '-', '.' or ':'".to_string(),
                ));
            }
            Some((key, value))
        }
        _ => {
            return Err(DuplicatesError::Invalid(
                "tag_key and tag_value go together".to_string(),
            ))
        }
    };
    let user_id = match trimmed(&q.user, "user")? {
        None => None,
        Some(name) => Some(
            UserRepository::find_by_name(db, &name)
                .await?
                .ok_or_else(|| DuplicatesError::Invalid(format!("unknown user '{name}'")))?
                .id,
        ),
    };
    Ok((
        DuplicateFilter {
            correlation_id_prefix: trimmed(&q.correlation_id_prefix, "correlation_id_prefix")?,
            tag,
            project: trimmed(&q.project, "project")?,
            user_id,
        },
        limit,
    ))
}

/// Run the report: resolve, query the window, total the groups.
pub async fn build_report(
    db: &dyn DatabaseProvider,
    q: &DuplicatesQuery,
) -> Result<DuplicatesReport, DuplicatesError> {
    let (filter, limit) = resolve(db, q).await?;
    let (start, end) = super::attribution::window_range(&q.window);
    let groups = CostRepository::duplicate_requests(db, &filter, &start, &end, limit).await?;
    let totals = groups.iter().fold(DuplicateTotals::default(), |mut t, g| {
        t.groups += 1;
        t.requests += g.requests;
        t.repeats += g.requests - 1;
        t.cache_hits += g.cache_hits;
        t.cost_usd += g.cost_usd;
        t.saved_usd += g.saved_usd;
        t
    });
    Ok(DuplicatesReport {
        window: q.window.clone(),
        start,
        end,
        filter,
        limit,
        totals,
        groups,
    })
}

/// GET /admin/api/usage/duplicates?window=&correlation_id_prefix=&tag_key=&tag_value=&project=&user=&limit=
pub async fn get_duplicates(
    State(state): State<AppState>,
    _session: AdminSession,
    Query(q): Query<DuplicatesQuery>,
) -> Result<Json<DuplicatesReport>, ApiError> {
    Ok(Json(build_report(&*state.db, &q).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> crate::db::sqlite::SqliteDb {
        let db = crate::db::sqlite::SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    #[tokio::test]
    async fn resolve_refuses_a_bad_window_limit_half_tag_and_unknown_user() {
        let db = db().await;
        let db = &db;
        let bad = |q: DuplicatesQuery| async move {
            match resolve(db, &q).await {
                Err(DuplicatesError::Invalid(msg)) => msg,
                other => panic!("expected Invalid, got {other:?}"),
            }
        };
        let base = DuplicatesQuery { window: "all".into(), ..Default::default() };
        assert!(bad(DuplicatesQuery { window: "yearly".into(), ..base.clone() }).await.contains("window"));
        assert!(bad(DuplicatesQuery { limit: Some(0), ..base.clone() }).await.contains("limit"));
        assert!(bad(DuplicatesQuery { limit: Some(MAX_LIMIT + 1), ..base.clone() }).await.contains("limit"));
        assert!(bad(DuplicatesQuery { tag_key: Some("k".into()), ..base.clone() }).await.contains("together"));
        assert!(bad(DuplicatesQuery { tag_key: Some("bad key".into()), tag_value: Some("v".into()), ..base.clone() })
            .await
            .contains("tag_key"));
        assert!(bad(DuplicatesQuery { user: Some("nobody".into()), ..base.clone() }).await.contains("unknown user"));
        assert!(bad(DuplicatesQuery { project: Some("p".repeat(MAX_VALUE_LEN + 1)), ..base.clone() })
            .await
            .contains("project"));
    }

    #[tokio::test]
    async fn resolve_trims_defaults_and_names_the_user() {
        let db = db().await;
        let alice = UserRepository::create(&db, crate::db::models::NewUser { name: "alice".into(), email: None })
            .await
            .unwrap()
            .id;
        let (filter, limit) = resolve(
            &db,
            &DuplicatesQuery {
                window: "monthly".into(),
                correlation_id_prefix: Some("  run-7 ".into()),
                tag_key: Some("team".into()),
                tag_value: Some(" blue".into()),
                project: Some("".into()),
                user: Some("alice".into()),
                limit: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(limit, DEFAULT_LIMIT);
        assert_eq!(
            filter,
            DuplicateFilter {
                correlation_id_prefix: Some("run-7".into()),
                tag: Some(("team".into(), "blue".into())),
                project: None,
                user_id: Some(alice),
            }
        );
    }

    #[test]
    fn errors_map_to_a_400_or_a_500() {
        assert!(matches!(
            ApiError::from(DuplicatesError::Invalid("x".into())),
            ApiError::InvalidRequest(m) if m == "x"
        ));
        assert!(matches!(
            ApiError::from(DuplicatesError::Db(anyhow::anyhow!("boom"))),
            ApiError::Internal
        ));
    }
}
