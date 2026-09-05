use std::collections::BTreeMap;

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
    /// Tokens served from the provider's prompt cache (billed at a reduced rate).
    pub cache_read_tokens: i64,
    /// Tokens written to the provider's prompt cache on this request.
    pub cache_write_tokens: i64,
    pub cost_usd: f64,
    pub latency_ms: Option<i64>,
    pub tags: String,
    pub project: Option<String>,
    /// Caller-supplied correlation id for this request. See `api::attribution`.
    #[sqlx(default)]
    #[serde(default)]
    pub attribution_correlation_id: Option<String>,
    /// Caller-supplied attribution tags as a JSON object; `{}` when absent.
    #[sqlx(default)]
    #[serde(default = "empty_json_object")]
    pub attribution_tags: String,
    /// Experiment this request was bound to, and the variant label, when the
    /// caller sent `x-modelrouter-experiment`. See `db::models::Experiment`.
    #[sqlx(default)]
    #[serde(default)]
    pub experiment_id: Option<i64>,
    #[sqlx(default)]
    #[serde(default)]
    pub experiment_variant: Option<String>,
    pub created_at: String,
}

pub(crate) fn empty_json_object() -> String {
    "{}".to_string()
}

impl Prompt {
    /// A prompt is considered "cached" if any tokens were served from cache.
    pub fn is_cached(&self) -> bool {
        self.cache_read_tokens > 0
    }
}

/// Where a request died. See migrations/024_request_failures.sql.
///
/// Kept as a small closed enum rather than a free string so a failure can be
/// grouped and alerted on: "resolve failures spiked" is actionable, "some
/// requests failed" is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureStage {
    /// Model/provider resolution — unknown provider, no configured adapter.
    Resolve,
    /// Budget, rate limit, guardrail or declarative policy denial.
    Policy,
    /// The upstream provider was reached and returned an error.
    Provider,
    /// The caller's request was malformed or unacceptable.
    Request,
    /// Anything else raised inside the router.
    Internal,
}

impl FailureStage {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureStage::Resolve => "resolve",
            FailureStage::Policy => "policy",
            FailureStage::Provider => "provider",
            FailureStage::Request => "request",
            FailureStage::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RequestFailure {
    pub id: i64,
    pub user_id: Option<i64>,
    pub api_key_id: Option<i64>,
    pub endpoint: String,
    pub request_model: String,
    pub routed_model: Option<String>,
    pub provider: Option<String>,
    pub stage: String,
    pub status_code: Option<i64>,
    pub error_message: String,
    pub attempts: i64,
    pub latency_ms: Option<i64>,
    pub project: Option<String>,
    pub attribution_correlation_id: Option<String>,
    pub attribution_tags: String,
    /// Experiment binding of the failed request, when there was one.
    #[sqlx(default)]
    #[serde(default)]
    pub experiment_id: Option<i64>,
    #[sqlx(default)]
    #[serde(default)]
    pub experiment_variant: Option<String>,
    pub created_at: String,
}

/// A failure to record. Deliberately carries NO prompt or response body: a
/// failure record must never become a second, unlogged copy of content that
/// `X-No-Log: true` was used to suppress.
#[derive(Debug, Clone)]
pub struct NewRequestFailure {
    pub user_id: Option<i64>,
    pub api_key_id: Option<i64>,
    pub endpoint: String,
    pub request_model: String,
    pub routed_model: Option<String>,
    pub provider: Option<String>,
    pub stage: FailureStage,
    pub status_code: Option<i64>,
    pub error_message: String,
    pub attempts: i64,
    pub latency_ms: Option<i64>,
    pub project: Option<String>,
    pub attribution_correlation_id: Option<String>,
    pub attribution_tags: String,
    pub experiment_id: Option<i64>,
    pub experiment_variant: Option<String>,
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
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: f64,
    pub latency_ms: Option<i64>,
    pub tags: String,
    pub project: Option<String>,
    pub attribution_correlation_id: Option<String>,
    pub attribution_tags: String,
    pub experiment_id: Option<i64>,
    pub experiment_variant: Option<String>,
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
    /// True when this usage record was served from the response cache. Such a
    /// row always has `cost_usd = 0` — it is usage, not spend.
    #[sqlx(default)]
    #[serde(default)]
    pub cache_hit: bool,
    /// Provider cost avoided by serving this row from cache. Zero on real calls.
    #[sqlx(default)]
    #[serde(default)]
    pub saved_usd: f64,
    /// Caller-supplied correlation id, denormalised from the request so cost
    /// queries never need to join back through `prompts` — which matters
    /// because skip-log and cache-hit rows have `prompt_id IS NULL`.
    #[sqlx(default)]
    #[serde(default)]
    pub attribution_correlation_id: Option<String>,
    /// Caller-supplied attribution tags as a JSON object; `{}` when absent.
    #[sqlx(default)]
    #[serde(default = "empty_json_object")]
    pub attribution_tags: String,
    /// Experiment binding of the request that produced this row, when any.
    #[sqlx(default)]
    #[serde(default)]
    pub experiment_id: Option<i64>,
    #[sqlx(default)]
    #[serde(default)]
    pub experiment_variant: Option<String>,
    /// True when the provider reported no usage and the token counts were
    /// estimated locally, so aggregates can say how much is measured.
    #[sqlx(default)]
    #[serde(default)]
    pub tokens_estimated: bool,
}

#[derive(Debug, Clone)]
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
    pub attribution_correlation_id: Option<String>,
    pub attribution_tags: String,
    pub experiment_id: Option<i64>,
    pub experiment_variant: Option<String>,
    pub tokens_estimated: bool,
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
    /// Who registered it. `None` for rows created before ownership existed;
    /// those are not mutable through the key-authenticated API.
    pub owner_user_id: Option<i64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewMcpServer {
    pub name: String,
    pub url: String,
    pub description: Option<String>,
    #[serde(skip)]
    pub owner_user_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: i64,
    pub provider: String,
    pub name: String,
    pub alias: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    /// Why an operator disabled this model (issue #5). `None` while enabled.
    pub disabled_reason: Option<String>,
    pub disabled_by: Option<String>,
    pub disabled_at: Option<String>,
}

/// Whole-provider enable/disable state (issue #5). Absence of a row means enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderState {
    pub provider: String,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
    pub disabled_by: Option<String>,
    pub disabled_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug)]
pub struct NewModel {
    pub provider: String,
    pub name: String,
    pub alias: Option<String>,
}

/// A runtime-managed alias row (issue #9). Overrides `models.alias` and config aliases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAlias {
    pub alias: String,
    pub target: String,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewModelAlias {
    pub alias: String,
    pub target: String,
    pub created_by: Option<String>,
}

/// Where one variant sends a requested model (spec §7a).
///
/// `target` is the expression the operator wrote (an alias or `provider/model`);
/// `provider` and `model` are what it resolved to when the experiment was
/// created. Binding uses the resolved pair, so an alias edit afterwards changes
/// ordinary traffic but never an active experiment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariantTarget {
    pub target: String,
    pub provider: String,
    pub model: String,
}

/// Variant label -> (requested model -> pinned target).
pub type ExperimentVariants = BTreeMap<String, BTreeMap<String, VariantTarget>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExperimentStatus {
    Active,
    Closed,
}

impl ExperimentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ExperimentStatus::Active => "active",
            ExperimentStatus::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "active" => Ok(ExperimentStatus::Active),
            "closed" => Ok(ExperimentStatus::Closed),
            other => anyhow::bail!("unknown experiment status: {other}"),
        }
    }
}

/// A controlled experiment (spec §7a). See migrations/029_experiments.sql.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Experiment {
    pub id: i64,
    pub name: String,
    pub variants: ExperimentVariants,
    /// User ids allowed to bind. Empty means every key may bind.
    pub allowed_user_ids: Vec<i64>,
    pub status: ExperimentStatus,
    /// Stored and returned for the later learning work; nothing reads it yet.
    pub feed_learning: bool,
    /// Unix seconds; 0 means never.
    pub expires_at: i64,
    pub created_at: String,
    pub closed_at: Option<String>,
    pub retain_content: bool,
    /// Days after close that retained content is kept; 0 means forever.
    pub content_retention_days: i64,
}

/// An experiment to create. `expires_at` and `content_retention_days` are
/// deliberately not defaulted anywhere: the caller must say `0` to mean never.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewExperiment {
    pub name: String,
    pub variants: ExperimentVariants,
    pub allowed_user_ids: Vec<i64>,
    pub feed_learning: bool,
    pub expires_at: i64,
    pub retain_content: bool,
    pub content_retention_days: i64,
}

/// The reported result of one run, keyed by user and correlation id. See
/// migrations/029_experiments.sql.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct RunOutcome {
    pub user_id: i64,
    pub attribution_correlation_id: String,
    /// `success` or `failure`.
    pub outcome: String,
    pub score: Option<f64>,
    pub rating: Option<i64>,
    /// Bounded metadata; never prompt or response content.
    pub note: Option<String>,
    pub experiment_id: Option<i64>,
    pub experiment_variant: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewRunOutcome {
    pub user_id: i64,
    pub attribution_correlation_id: String,
    pub outcome: String,
    pub score: Option<f64>,
    pub rating: Option<i64>,
    pub note: Option<String>,
    pub experiment_id: Option<i64>,
    pub experiment_variant: Option<String>,
}

/// The experiment binding of a run, read from its earliest stamped ledger row.
/// Both fields are `None` when the run has ledger rows but none was stamped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunStamp {
    pub experiment_id: Option<i64>,
    pub experiment_variant: Option<String>,
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
            owner_user_id: None,
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
