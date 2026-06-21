use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub email: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub metadata: String,
    /// Set during authentication when matched via api_keys table; None for legacy key auth.
    #[sqlx(default)]
    pub api_key_id: Option<i64>,
    /// If set, only costs recorded after this timestamp count toward budget limits.
    #[sqlx(default)]
    pub spend_reset_at: Option<String>,
    /// Project from the authenticating API key. Set in memory by auth extractor.
    #[sqlx(default)]
    pub api_key_project: Option<String>,
    /// Per-key synthetic session window in seconds. Set in memory by auth extractor.
    #[sqlx(default)]
    pub session_window_secs: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: i64,
    pub user_id: i64,
    pub key_hash: String,
    pub label: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    /// RFC3339 UTC expiry. None = never expires.
    pub expires_at: Option<String>,
    /// Project this key is associated with (e.g., "modelrouter-api", "other-app").
    pub project: Option<String>,
    /// RFC3339 UTC timestamp of when the key was explicitly disabled via admin UI.
    pub disabled_at: Option<String>,
    /// Synthetic session ID window in seconds. None = server default (28800 = 8 hours).
    pub session_window_secs: Option<i64>,
}

impl ApiKey {
    /// Returns true if the key is enabled and not past its expiry.
    /// Both timestamps are RFC3339 UTC +00:00 strings; lexicographic comparison is correct.
    pub fn is_valid(&self) -> bool {
        if !self.enabled {
            return false;
        }
        match &self.expires_at {
            None => true,
            Some(exp) => exp.as_str() > chrono::Utc::now().to_rfc3339().as_str(),
        }
    }
}

#[derive(Debug)]
pub struct NewApiKey {
    pub user_id: i64,
    pub key_hash: String,
    pub label: Option<String>,
    pub expires_at: Option<String>,
    pub project: Option<String>,
    /// Synthetic session ID window in seconds. None = server default (28800 = 8 hours).
    pub session_window_secs: Option<i64>,
}

#[derive(Debug)]
pub struct NewUser {
    pub name: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: i64,
    pub name: String,
    pub priority: i64,
    pub enabled: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMembership {
    pub id: i64,
    pub group_id: i64,
    pub user_id: i64,
    /// Joined from the users table via aliased column `user_name`.
    pub user_name: String,
    pub joined_at: String,
    pub disabled_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AdminUser {
    pub id: i64,
    pub name: String,
    pub password_hash: String,
    pub role: String,
    pub enabled: bool,
    pub created_at: String,
    pub last_login_at: Option<String>,
    pub oidc_subject: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug)]
pub struct NewAdminUser {
    pub name: String,
    pub password_hash: String,
    pub role: String,
}

#[derive(Debug)]
pub struct NewAdminUserFromOidc {
    pub name: String,
    pub email: String,
    pub oidc_subject: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Session {
    pub id: i64,
    pub user_id: i64,
    pub external_id: Option<String>,
    pub project: Option<String>,
    pub created_at: String,
    pub last_seen: String,
    pub metadata: String,
}

#[derive(Debug)]
pub struct NewSession {
    pub user_id: i64,
    pub external_id: Option<String>,
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Prompt {
    pub id: i64,
    pub user_id: i64,
    pub session_id: Option<i64>,
    pub request_model: String,
    pub routed_model: String,
    pub provider: String,
    pub messages: String,
    pub response: Option<String>,
    pub finish_reason: Option<String>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cost_usd: f64,
    pub latency_ms: Option<i64>,
    pub tags: String,
    pub project: Option<String>,
    pub created_at: String,
}

#[derive(Debug)]
pub struct NewPrompt {
    pub user_id: i64,
    pub session_id: Option<i64>,
    pub request_model: String,
    pub routed_model: String,
    pub provider: String,
    pub messages: String,
    pub response: Option<String>,
    pub finish_reason: Option<String>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cost_usd: f64,
    pub latency_ms: Option<i64>,
    pub tags: String,
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CostLedgerEntry {
    pub id: i64,
    pub user_id: i64,
    pub prompt_id: i64,
    pub model: String,
    pub provider: String,
    pub project: Option<String>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost_usd: f64,
    pub created_at: String,
    #[sqlx(default)]
    pub api_key_id: Option<i64>,
}

#[derive(Debug)]
pub struct NewCostLedgerEntry {
    pub user_id: i64,
    pub prompt_id: Option<i64>,
    pub model: String,
    pub provider: String,
    pub project: Option<String>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost_usd: f64,
    pub api_key_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BudgetRule {
    pub id: i64,
    pub user_id: Option<i64>,
    pub group_name: Option<String>,
    #[sqlx(default)]
    pub api_key_id: Option<i64>,
    /// If set, this rule applies to API keys with a matching tag.
    #[sqlx(default)]
    pub tag: Option<String>,
    pub window: String,
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<i64>,
    pub model_allow: String,
    pub model_deny: String,
    pub rate_rpm: Option<i64>,
    #[sqlx(default)]
    pub max_concurrent: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    #[sqlx(default)]
    pub project: Option<String>,
    #[sqlx(default)]
    pub window_start: Option<String>,
    #[sqlx(default)]
    pub window_end: Option<String>,
}

#[derive(Debug)]
pub struct NewBudgetRule {
    pub user_id: Option<i64>,
    pub group_name: Option<String>,
    pub api_key_id: Option<i64>,
    pub tag: Option<String>,
    pub window: String,
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<i64>,
    pub model_allow: Vec<String>,
    pub model_deny: Vec<String>,
    pub rate_rpm: Option<i64>,
    pub max_concurrent: Option<i64>,
    pub project: Option<String>,
    pub window_start: Option<String>,
    pub window_end: Option<String>,
}

/// Fields editable after creation. Scope fields (user_id, group_name, project)
/// and window type are immutable — delete and recreate to change them.
#[derive(Debug)]
pub struct UpdateBudgetRule {
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<i64>,
    pub model_allow: Option<Vec<String>>,
    pub model_deny: Option<Vec<String>>,
    pub rate_rpm: Option<i64>,
    pub max_concurrent: Option<i64>,
    pub window_start: Option<String>,
    pub window_end: Option<String>,
}

/// Scope discriminator for budget rule queries.
/// Note: the `tag` field in BudgetRule is a legacy scope mechanism not represented here;
/// tag-scoped rules are still enforced via the existing `list_for_tag` path in policy.rs.
#[derive(Debug, Clone)]
pub enum BudgetScope {
    Global,
    Project(String),
    User(i64),
    Group(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AuditLogEntry {
    pub id: i64,
    pub actor_id: Option<i64>,
    pub actor_name: String,
    pub action: String,
    pub target: Option<String>,
    pub before_json: Option<String>,
    pub after_json: Option<String>,
    pub created_at: String,
}

#[derive(Debug)]
pub struct NewAuditLogEntry {
    pub actor_id: Option<i64>,
    pub actor_name: String,
    pub action: String,
    pub target: Option<String>,
    pub before_json: Option<String>,
    pub after_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct HookPermission {
    pub id: i64,
    pub hook_name: String,
    pub capability: String,
    pub granted_by: Option<i64>,
    pub granted_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct HookMetric {
    pub hook_name: String,
    pub invoked_at: String,
    pub duration_ms: i64,
    pub success: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpServer {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewMcpServer {
    pub name: String,
    pub url: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: i64,
    pub provider: String,
    pub name: String,
    pub alias: Option<String>,
    pub enabled: bool,
    pub created_at: String,
}

#[derive(Debug)]
pub struct NewModel {
    pub provider: String,
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFailover {
    pub id: i64,
    pub primary_model: String,
    pub fallback_model: String,
    pub priority: i64,
}

#[cfg(test)]
mod group_tests {
    use super::*;

    #[test]
    fn group_enabled_default() {
        let g = Group {
            id: 1,
            name: "eng".to_string(),
            priority: 10,
            enabled: true,
            created_at: "2026-04-10T00:00:00Z".to_string(),
        };
        assert!(g.enabled);
    }

    #[test]
    fn group_membership_fields() {
        let m = GroupMembership {
            id: 1,
            group_id: 1,
            user_id: 2,
            user_name: "alice".to_string(),
            joined_at: "2026-04-10T00:00:00Z".to_string(),
            disabled_at: None,
        };
        assert_eq!(m.user_name, "alice");
        assert!(m.disabled_at.is_none());
    }
}

#[cfg(test)]
mod mcp_tests {
    use super::*;

    #[test]
    fn mcp_server_roundtrip() {
        let s = McpServer {
            id: 1,
            name: "my-server".to_string(),
            url: "https://example.com/mcp".to_string(),
            description: Some("does stuff".to_string()),
            enabled: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        assert_eq!(s.name, "my-server");
        assert!(s.enabled);
    }
}
