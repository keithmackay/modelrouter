//! Admin API for scoped alias overrides (see `router::scoped_aliases`).
//!
//! A scope is one attribution tag, `tag_key = tag_value`. Writing a scope
//! replaces its whole set of overrides at once, so a reader never sees half
//! of an update. Targets are pinned at write time through the same gate as
//! experiment variants: no load balancer pool, no default-model
//! substitution, a configured provider, and a pricing entry, so every call an
//! override routes is costed.
//!
//! Reads need an admin session; writes need a superadmin, and are audited.

use std::collections::BTreeMap;

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::audit::audit;
use super::auth::{AdminSession, SuperAdminSession};
use super::experiments::{parse_expires_at, pin_target, GateSources};
use crate::api::{app::AppState, error::ApiError};
use crate::db::models::{NewScopedAlias, ScopedAlias};

/// Attribution tag limits (`api::attribution`): a scope must be a tag a
/// request could actually carry.
const MAX_TAG_KEY_LEN: usize = 64;
const MAX_TAG_VALUE_LEN: usize = 128;
/// More aliases than any caller has tiers; a bound on the write, not a policy.
const MAX_ALIASES_PER_SCOPE: usize = 32;

/// One alias of a scope, as the API shows it.
#[derive(Debug, Serialize)]
pub struct ScopedAliasView {
    pub target: String,
    pub provider: String,
    pub model: String,
}

/// One scope and its overrides.
#[derive(Debug, Serialize)]
pub struct ScopeView {
    pub tag_key: String,
    pub tag_value: String,
    /// RFC3339, or "never".
    pub expires_at: String,
    pub expired: bool,
    pub created_by: Option<String>,
    pub created_at: String,
    pub aliases: BTreeMap<String, ScopedAliasView>,
}

fn expiry_text(expires_at: i64) -> String {
    if expires_at == 0 {
        return "never".to_string();
    }
    chrono::DateTime::from_timestamp(expires_at, 0)
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| expires_at.to_string())
}

/// Group rows (already ordered by scope then alias) into scopes.
pub fn group_scopes(rows: Vec<ScopedAlias>, now_epoch: i64) -> Vec<ScopeView> {
    let mut scopes: Vec<ScopeView> = Vec::new();
    for row in rows {
        let same = scopes
            .last()
            .is_some_and(|s| s.tag_key == row.tag_key && s.tag_value == row.tag_value);
        if !same {
            scopes.push(ScopeView {
                tag_key: row.tag_key.clone(),
                tag_value: row.tag_value.clone(),
                expires_at: expiry_text(row.expires_at),
                expired: row.expires_at != 0 && now_epoch >= row.expires_at,
                created_by: row.created_by.clone(),
                created_at: row.created_at.clone(),
                aliases: BTreeMap::new(),
            });
        }
        let scope = scopes.last_mut().expect("pushed above");
        scope.aliases.insert(
            row.alias,
            ScopedAliasView { target: row.target, provider: row.provider, model: row.model },
        );
    }
    scopes
}

fn validate_scope(tag_key: &str, tag_value: &str) -> Result<(), String> {
    if tag_key.is_empty() || tag_key.len() > MAX_TAG_KEY_LEN {
        return Err(format!("tag_key must be 1-{MAX_TAG_KEY_LEN} characters"));
    }
    if tag_value.is_empty() || tag_value.len() > MAX_TAG_VALUE_LEN {
        return Err(format!("tag_value must be 1-{MAX_TAG_VALUE_LEN} characters"));
    }
    Ok(())
}

/// Parsed body of a scope write: alias -> target expression, and expiry.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedScopeWrite {
    pub aliases: BTreeMap<String, String>,
    pub expires_at: i64,
}

/// `{"aliases": {"<alias>": "<target>", ...}, "expires_at": "<RFC3339>" | 0}`.
/// Shape and bounds only; targets are pinned by the handler.
pub fn parse_scope_write(body: &Value, now: chrono::DateTime<chrono::Utc>) -> Result<ParsedScopeWrite, String> {
    let map = body
        .get("aliases")
        .and_then(Value::as_object)
        .ok_or_else(|| "aliases must be an object of alias -> target".to_string())?;
    if map.is_empty() {
        return Err("aliases must name at least one alias; DELETE the scope to clear it".to_string());
    }
    if map.len() > MAX_ALIASES_PER_SCOPE {
        return Err(format!("aliases may name at most {MAX_ALIASES_PER_SCOPE} aliases"));
    }
    let mut aliases = BTreeMap::new();
    for (alias, target) in map {
        let target = target
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("aliases: '{alias}' must map to a non-empty target"))?;
        let alias = alias.trim();
        if alias.is_empty() || alias.starts_with(':') {
            return Err(format!("aliases: '{alias}' is not a valid alias name"));
        }
        if alias == target {
            return Err(format!("aliases: '{alias}' cannot point at itself"));
        }
        aliases.insert(alias.to_string(), target.to_string());
    }
    let expires_at = parse_expires_at(body, now)?;
    Ok(ParsedScopeWrite { aliases, expires_at })
}

/// Reload the live overrides from the database. Called after every write, at
/// startup and on the lifecycle tick. A failed reload keeps the previous
/// snapshot and is logged; the tick retries.
pub async fn refresh_scoped_aliases(state: &AppState) {
    match state.db.list_scoped_aliases().await {
        Ok(rows) => state.router.scoped_aliases().store(&rows),
        Err(e) => tracing::warn!(error = %e, "failed to reload scoped alias overrides"),
    }
}

/// Delete expired overrides, audit each scope removed, and reload.
pub async fn expire_scoped_aliases(state: &AppState, now_epoch: i64) {
    match state.db.delete_expired_scoped_aliases(now_epoch).await {
        Ok(rows) if !rows.is_empty() => {
            for scope in group_scopes(rows, now_epoch) {
                audit(
                    &state.db,
                    None,
                    "system",
                    "scoped_alias.expire",
                    Some(scope_target(&scope.tag_key, &scope.tag_value)),
                    serde_json::to_string(&scope).ok(),
                    None,
                )
                .await;
            }
            refresh_scoped_aliases(state).await;
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "failed to delete expired scoped alias overrides"),
    }
}

fn scope_target(tag_key: &str, tag_value: &str) -> String {
    format!("scope:{tag_key}={tag_value}")
}

/// Pin every alias of a scope write. Synchronous, so the gate (which is not
/// `Send`) is never held across an await.
fn pin_scope(state: &AppState, parsed: &ParsedScopeWrite) -> Result<Vec<NewScopedAlias>, String> {
    let gate = GateSources::from_state(state);
    parsed
        .aliases
        .iter()
        .map(|(alias, expr)| {
            let target = pin_target(&gate, &format!("aliases: '{alias}' target '{expr}'"), expr)?;
            Ok(NewScopedAlias { alias: alias.clone(), target: target.target, provider: target.provider, model: target.model })
        })
        .collect()
}

async fn scope_rows(state: &AppState, tag_key: &str, tag_value: &str) -> Result<Vec<ScopedAlias>, ApiError> {
    Ok(state
        .db
        .list_scoped_aliases()
        .await
        .map_err(|_| ApiError::Internal)?
        .into_iter()
        .filter(|r| r.tag_key == tag_key && r.tag_value == tag_value)
        .collect())
}

#[derive(Deserialize)]
pub struct ScopeFilter {
    pub tag_key: Option<String>,
    pub tag_value: Option<String>,
}

/// GET /admin/api/scoped-aliases[?tag_key=&tag_value=]
pub async fn list_scoped_aliases_api(
    State(state): State<AppState>,
    _session: AdminSession,
    Query(filter): Query<ScopeFilter>,
) -> Result<impl IntoResponse, ApiError> {
    let rows = state.db.list_scoped_aliases().await.map_err(|_| ApiError::Internal)?;
    let rows = rows
        .into_iter()
        .filter(|r| filter.tag_key.as_deref().map_or(true, |k| r.tag_key == k))
        .filter(|r| filter.tag_value.as_deref().map_or(true, |v| r.tag_value == v))
        .collect();
    Ok(Json(json!({ "scopes": group_scopes(rows, chrono::Utc::now().timestamp()) })))
}

/// PUT /admin/api/scoped-aliases/:tag_key/:tag_value — replace the scope.
pub async fn put_scoped_aliases_api(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Path((tag_key, tag_value)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    validate_scope(&tag_key, &tag_value).map_err(ApiError::InvalidRequest)?;
    let now = chrono::Utc::now();
    let parsed = parse_scope_write(&body, now).map_err(ApiError::InvalidRequest)?;
    let pinned = pin_scope(&state, &parsed).map_err(ApiError::InvalidRequest)?;

    let before = scope_rows(&state, &tag_key, &tag_value).await?;
    let rows = state
        .db
        .replace_scoped_aliases(&tag_key, &tag_value, &pinned, parsed.expires_at, Some(&session.0.name))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to write scoped alias overrides");
            ApiError::Internal
        })?;
    refresh_scoped_aliases(&state).await;

    let now_epoch = now.timestamp();
    let before = group_scopes(before, now_epoch).pop();
    let after = group_scopes(rows, now_epoch).pop();
    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        if before.is_some() { "scoped_alias.update" } else { "scoped_alias.create" },
        Some(scope_target(&tag_key, &tag_value)),
        before.as_ref().and_then(|b| serde_json::to_string(b).ok()),
        after.as_ref().and_then(|a| serde_json::to_string(a).ok()),
    )
    .await;
    Ok(Json(json!({ "scope": after })))
}

/// DELETE /admin/api/scoped-aliases/:tag_key/:tag_value — clear the scope.
pub async fn delete_scoped_aliases_api(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Path((tag_key, tag_value)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let before = scope_rows(&state, &tag_key, &tag_value).await?;
    if before.is_empty() {
        // Idempotent: clearing an empty scope is not an error, and not audited.
        return Ok(Json(json!({ "deleted": 0 })));
    }
    state
        .db
        .replace_scoped_aliases(&tag_key, &tag_value, &[], 0, None)
        .await
        .map_err(|_| ApiError::Internal)?;
    refresh_scoped_aliases(&state).await;
    let before = group_scopes(before, chrono::Utc::now().timestamp()).pop();
    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "scoped_alias.delete",
        Some(scope_target(&tag_key, &tag_value)),
        before.as_ref().and_then(|b| serde_json::to_string(b).ok()),
        None,
    )
    .await;
    Ok(Json(json!({ "deleted": before.map(|b| b.aliases.len()).unwrap_or(0) })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z").unwrap().with_timezone(&chrono::Utc)
    }

    #[test]
    fn parses_aliases_and_expiry() {
        let parsed = parse_scope_write(
            &json!({ "aliases": { "deep": " mock/model-b ", "fast": "mock/model-a" }, "expires_at": 0 }),
            now(),
        )
        .unwrap();
        assert_eq!(parsed.expires_at, 0);
        assert_eq!(parsed.aliases["deep"], "mock/model-b");
        assert_eq!(parsed.aliases.len(), 2);
    }

    #[test]
    fn refuses_bad_bodies_naming_the_field() {
        let cases = [
            (json!({ "expires_at": 0 }), "aliases must be an object"),
            (json!({ "aliases": {}, "expires_at": 0 }), "DELETE the scope"),
            (json!({ "aliases": { "deep": "" }, "expires_at": 0 }), "non-empty target"),
            (json!({ "aliases": { ":fastest": "a/b" }, "expires_at": 0 }), "not a valid alias"),
            (json!({ "aliases": { "deep": "deep" }, "expires_at": 0 }), "cannot point at itself"),
            (json!({ "aliases": { "deep": "a/b" } }), "expires_at"),
            (json!({ "aliases": { "deep": "a/b" }, "expires_at": "2020-01-01T00:00:00Z" }), "in the future"),
        ];
        for (body, expected) in cases {
            let err = parse_scope_write(&body, now()).unwrap_err();
            assert!(err.contains(expected), "{body}: {err}");
        }
    }

    #[test]
    fn scope_bounds_match_attribution_limits() {
        assert!(validate_scope("tenant", "t1").is_ok());
        assert!(validate_scope("", "t1").is_err());
        assert!(validate_scope("tenant", &"v".repeat(129)).is_err());
        assert!(validate_scope(&"k".repeat(65), "t1").is_err());
    }

    #[test]
    fn groups_rows_into_scopes_with_expiry_spelled_out() {
        let row = |key: &str, alias: &str, expires_at: i64| ScopedAlias {
            tag_key: key.into(),
            tag_value: "v".into(),
            alias: alias.into(),
            target: "mock/m".into(),
            provider: "mock".into(),
            model: "m".into(),
            expires_at,
            created_by: None,
            created_at: String::new(),
        };
        let scopes = group_scopes(vec![row("a", "deep", 0), row("a", "fast", 0), row("b", "deep", 10)], 20);
        assert_eq!(scopes.len(), 2);
        assert_eq!(scopes[0].aliases.len(), 2);
        assert_eq!(scopes[0].expires_at, "never");
        assert!(!scopes[0].expired);
        assert!(scopes[1].expired);
    }
}
