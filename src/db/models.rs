use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub api_key: String,
    pub api_key_old: Option<String>,
    pub api_key_old_expires_at: Option<String>,
    pub group_name: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub metadata: String,
    /// Set during authentication when matched via api_keys table; None for legacy key auth.
    #[sqlx(default)]
    pub api_key_id: Option<i64>,
    /// If set, only costs recorded after this timestamp count toward budget limits.
    #[sqlx(default)]
    pub spend_reset_at: Option<String>,
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
}

#[derive(Debug)]
pub struct NewUser {
    pub name: String,
    pub api_key_hash: String,
    pub group_name: Option<String>,
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
}

#[derive(Debug)]
pub struct NewAdminUser {
    pub name: String,
    pub password_hash: String,
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
    pub prompt_id: i64,
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
    pub window: String,
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<i64>,
    pub model_allow: String,
    pub model_deny: String,
    pub rate_rpm: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug)]
pub struct NewBudgetRule {
    pub user_id: Option<i64>,
    pub group_name: Option<String>,
    pub api_key_id: Option<i64>,
    pub window: String,
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<i64>,
    pub model_allow: Vec<String>,
    pub model_deny: Vec<String>,
    pub rate_rpm: Option<i64>,
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
