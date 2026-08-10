// src/config/schema.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PricingEntry {
    pub model: String,
    pub input_per_million: f64,
    pub output_per_million: f64,
    /// Rate for prompt-cache reads. Defaults to 10% of `input_per_million` if unset.
    #[serde(default)]
    pub cache_read_per_million: Option<f64>,
    /// Rate for prompt-cache writes. Defaults to 125% of `input_per_million` if unset.
    #[serde(default)]
    pub cache_write_per_million: Option<f64>,
}

/// Response cache configuration.
///
/// The cache is exact-match only: identical eligible requests are served from
/// the store at zero provider cost. Eligibility is deliberately conservative by
/// default — see [`CompletionCachePolicy`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CacheConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Store backend: `memory` (per-process, default) or `redis` (shared).
    #[serde(default = "default_cache_backend")]
    pub backend: String,
    /// Redis connection URL, used when `backend = "redis"`.
    #[serde(default)]
    pub redis_url: String,
    /// Key namespace. Keeps environments from sharing entries in one Redis.
    #[serde(default = "default_cache_namespace")]
    pub namespace: String,
    /// Max entries held by the in-memory backend. Ignored by Redis.
    #[serde(default = "default_cache_max_entries")]
    pub max_entries: u64,
    /// Fallback TTL for any class that does not set its own.
    #[serde(default = "default_cache_ttl")]
    pub ttl_seconds: u64,
    #[serde(default)]
    pub completions: CompletionCachePolicy,
    #[serde(default)]
    pub search: SearchCachePolicy,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: default_cache_backend(),
            redis_url: String::new(),
            namespace: default_cache_namespace(),
            max_entries: default_cache_max_entries(),
            ttl_seconds: default_cache_ttl(),
            completions: CompletionCachePolicy::default(),
            search: SearchCachePolicy::default(),
        }
    }
}

/// Eligibility policy for `/v1/chat/completions`.
///
/// Default is conservative: only requests whose `temperature` is explicitly at
/// or below `max_temperature` (0.0) are cached. A request that omits
/// `temperature` is scored using `assumed_temperature` (1.0, the OpenAI
/// default), so omitting it means *not cached* — creative sampling never
/// silently replays a stored answer.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompletionCachePolicy {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub max_temperature: f64,
    #[serde(default = "default_assumed_temperature")]
    pub assumed_temperature: f64,
    /// TTL override for completion entries. Falls back to `cache.ttl_seconds`.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

impl Default for CompletionCachePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_temperature: 0.0,
            assumed_temperature: default_assumed_temperature(),
            ttl_seconds: None,
        }
    }
}

/// Eligibility policy for `/v1/search`. Search results are deterministic enough
/// to cache by default, but go stale faster than completions, hence the shorter
/// default TTL.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SearchCachePolicy {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_search_cache_ttl")]
    pub ttl_seconds: u64,
}

impl Default for SearchCachePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl_seconds: default_search_cache_ttl(),
        }
    }
}

fn default_cache_max_entries() -> u64 { 1000 }
fn default_cache_ttl() -> u64 { 3600 }
fn default_cache_backend() -> String { "memory".to_string() }
fn default_cache_namespace() -> String { "modelrouter".to_string() }
fn default_assumed_temperature() -> f64 { 1.0 }
fn default_search_cache_ttl() -> u64 { 900 }
fn default_true() -> bool { true }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetryConfig {
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_retry_base_delay_ms")]
    pub base_delay_ms: u64,
    #[serde(default = "default_retry_max_delay_ms")]
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            base_delay_ms: default_retry_base_delay_ms(),
            max_delay_ms: default_retry_max_delay_ms(),
        }
    }
}

fn default_max_retries() -> u32 { 3 }
fn default_retry_base_delay_ms() -> u64 { 1000 }
fn default_retry_max_delay_ms() -> u64 { 30000 }

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SessionLimitConfig {
    /// Max tokens per minute per session. 0 = disabled.
    #[serde(default)]
    pub tpm: u32,
    /// Max requests per minute per session. 0 = disabled.
    #[serde(default)]
    pub rpm: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub pricing: Vec<PricingEntry>,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub session_limits: SessionLimitConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub callbacks: CallbacksConfig,
    #[serde(default)]
    pub guardrails: Vec<GuardrailConfig>,
    #[serde(default)]
    pub policy_rules: Vec<PolicyRuleConfig>,
    #[cfg(feature = "otel")]
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub archival: ArchivalConfig,
    #[serde(default)]
    pub oidc: OidcConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArchivalConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_archive_after_days")]
    pub after_days: u32,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default = "default_archive_prefix")]
    pub prefix: String,
    #[serde(default)]
    pub access_key: String,
    #[serde(default)]
    pub secret_key: String,
    #[serde(default = "default_archive_region")]
    pub region: String,
}

impl Default for ArchivalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            after_days: default_archive_after_days(),
            endpoint: String::new(),
            bucket: String::new(),
            prefix: default_archive_prefix(),
            access_key: String::new(),
            secret_key: String::new(),
            region: default_archive_region(),
        }
    }
}

fn default_archive_after_days() -> u32 { 90 }
fn default_archive_prefix() -> String { "modelrouter/cost-logs".to_string() }
fn default_archive_region() -> String { "us-east-1".to_string() }

fn default_oidc_role() -> String { "admin".to_string() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OidcConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub issuer_url: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default = "default_oidc_role")]
    pub auto_provision_role: String,
}

impl Default for OidcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            issuer_url: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            redirect_uri: String::new(),
            allowed_emails: vec![],
            allowed_domains: vec![],
            auto_provision_role: default_oidc_role(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_request_body_limit_mb")]
    pub request_body_limit_mb: usize,
    /// Max requests per minute per IP address. 0 = disabled (default).
    #[serde(default)]
    pub ip_rate_limit_rpm: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            request_body_limit_mb: default_request_body_limit_mb(),
            ip_rate_limit_rpm: 0,
        }
    }
}

fn default_host() -> String { "127.0.0.1".to_string() }
fn default_port() -> u16 { 8080 }
fn default_request_body_limit_mb() -> usize { 10 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_path")]
    pub path: String,
    #[serde(default)]
    pub postgres_url: Option<String>,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self { path: default_db_path(), postgres_url: None }
    }
}

fn default_db_path() -> String { "~/.modelrouter/router.db".to_string() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoutingConfig {
    #[serde(default = "default_provider")]
    pub default_provider: String,
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default)]
    pub model_aliases: HashMap<String, String>,
    #[serde(default)]
    pub fallback_chains: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub complexity_routing: Option<ComplexityRoutingConfig>,
    /// Named load balancer pools. Key is the virtual pool name used as `model` in requests.
    #[serde(default)]
    pub load_balancer: HashMap<String, LoadBalancerConfig>,
    #[serde(default)]
    pub shortcuts: RoutingShortcutsConfig,
    /// Reject a request whose model resolves to nothing, instead of silently
    /// serving `default_model`.
    ///
    /// Default `false` preserves the historical behaviour. Turning it on is
    /// strongly recommended for any caller that cares WHICH model answered:
    /// with it off, an unaliased name (e.g. `claude-opus-4-5-20251101`, which
    /// is neither an alias nor `provider/model`) falls through to the default
    /// and is answered by a different model entirely, recorded as a success.
    /// Observed live: 1,330 such requests served by `gpt-4o-mini` while the
    /// caller believed it was using Opus.
    #[serde(default)]
    pub strict_model_resolution: bool,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            default_provider: default_provider(),
            default_model: default_model(),
            model_aliases: HashMap::new(),
            fallback_chains: HashMap::new(),
            complexity_routing: None,
            load_balancer: HashMap::new(),
            shortcuts: RoutingShortcutsConfig::default(),
            strict_model_resolution: false,
        }
    }
}

fn default_provider() -> String { "openai".to_string() }
fn default_model() -> String { "gpt-4o".to_string() }

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ComplexityRoutingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_complexity_threshold")]
    pub token_threshold: u32,
    #[serde(default = "default_cheap_model")]
    pub cheap_model: String,
}

fn default_complexity_threshold() -> u32 { 500 }
fn default_cheap_model() -> String { "gpt-4o-mini".to_string() }

/// Reserved model name shortcuts resolved before alias lookup.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RoutingShortcutsConfig {
    /// Model string `:fastest` resolves to (e.g. "anthropic/claude-haiku-4-5").
    /// If None, `:fastest` falls through to default routing.
    pub fastest: Option<String>,
    /// Model string `:cheapest` resolves to (e.g. "deepseek/deepseek-chat").
    /// If None, `:cheapest` falls through to default routing.
    pub cheapest: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LbStrategy {
    RoundRobin,
    Weighted,
}

impl Default for LbStrategy {
    fn default() -> Self {
        Self::RoundRobin
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LbPoolEntry {
    pub provider: String,
    pub model: String,
    /// Weight for weighted round-robin. Higher values increase selection frequency.
    /// A weight of 0 silently excludes the entry from rotation.
    /// Default: 1
    #[serde(default = "default_lb_weight")]
    pub weight: u32,
}

fn default_lb_weight() -> u32 { 1 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoadBalancerConfig {
    #[serde(default)]
    pub strategy: LbStrategy,
    #[serde(default)]
    pub pool: Vec<LbPoolEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Azure OpenAI API version (e.g. "2024-02-01"). Used only by the Azure adapter.
    #[serde(default)]
    pub api_version: Option<String>,
    /// AWS region for Bedrock (e.g. "us-east-1"). Used only by the Bedrock adapter.
    /// Defaults to the AWS standard chain (AWS_REGION env var / ~/.aws/config).
    #[serde(default)]
    pub region: Option<String>,
    /// GCP project ID for Vertex AI requests. Used only by the Vertex adapter.
    /// If None, falls back to the project embedded in the service-account JSON
    /// or the ADC quota project.
    #[serde(default)]
    pub project: Option<String>,
    /// Path to GCP service-account JSON. If None, uses Application Default Credentials.
    /// Used only by the Vertex adapter.
    #[serde(default)]
    pub credentials_path: Option<String>,
    /// Region for the embedding endpoint, when it differs from `region`.
    ///
    /// Vertex serves `text-embedding-*` regionally only — `locations/global`
    /// has no embedding endpoint at all, while the Claude and Gemini chat models
    /// do run there. A Vertex provider configured for chat on `global` therefore
    /// has to name an embedding region separately, exactly as Athena's own
    /// client does with `EMBEDDING_REGION` (default `us-central1`) alongside its
    /// chat region. Falls back to `region` when unset.
    #[serde(default)]
    pub embedding_region: Option<String>,
    /// Vertex embedding task type (`RETRIEVAL_DOCUMENT`, `RETRIEVAL_QUERY`,
    /// `SEMANTIC_SIMILARITY`, …), sent as each instance's `task_type`.
    ///
    /// It lives in config rather than in code because it has no equivalent in
    /// the OpenAI embeddings API the caller speaks, and because getting it wrong
    /// is silent: query-typed and document-typed vectors coexist happily in one
    /// store and simply retrieve worse. Omitted from the request when unset.
    #[serde(default)]
    pub embedding_task_type: Option<String>,
}

impl Default for ProviderConfig {
    /// Must agree with the `#[serde(default)]` attributes above, so a
    /// `[providers.x]` table that sets nothing and a `ProviderConfig::default()`
    /// built in code describe the same provider. `tests/test_vertex.rs` pins
    /// that equivalence.
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_base: None,
            timeout_secs: default_timeout_secs(),
            api_version: None,
            region: None,
            project: None,
            credentials_path: None,
            embedding_region: None,
            embedding_task_type: None,
        }
    }
}

fn default_timeout_secs() -> u64 { 60 }

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct HooksConfig {
    #[serde(default)]
    pub lifecycle: Vec<LifecycleHookConfig>,
    #[serde(default)]
    pub pipeline: Vec<PipelineHookConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LifecycleHookConfig {
    pub name: String,
    pub event: String,
    pub exec: String,
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PipelineHookConfig {
    pub name: String,
    pub event: String,
    pub exec: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub fail_open: bool,
}

fn default_hook_timeout() -> u64 { 5 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthConfig {
    #[serde(default = "default_jwt_secret")]
    pub jwt_secret: String,
    #[serde(default = "default_jwt_expiry_mins")]
    pub jwt_expiry_mins: i64,
    #[serde(default = "default_rotation_overlap_mins")]
    pub rotation_overlap_mins: i64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            jwt_secret: default_jwt_secret(),
            jwt_expiry_mins: default_jwt_expiry_mins(),
            rotation_overlap_mins: default_rotation_overlap_mins(),
        }
    }
}

fn default_jwt_secret() -> String { "change-me-jwt-secret".to_string() }
fn default_jwt_expiry_mins() -> i64 { 60 }
fn default_rotation_overlap_mins() -> i64 { 15 }

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CallbacksConfig {
    #[serde(default)]
    pub langfuse: Option<LangFuseConfig>,
    #[serde(default)]
    pub langsmith: Option<LangSmithConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LangFuseConfig {
    pub public_key: String,
    pub secret_key: String,
    #[serde(default = "default_langfuse_host")]
    pub host: String,
}
fn default_langfuse_host() -> String { "https://cloud.langfuse.com".to_string() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LangSmithConfig {
    pub api_key: String,
    #[serde(default = "default_langsmith_host")]
    pub host: String,
    pub project: String,
}
fn default_langsmith_host() -> String { "https://api.smith.langchain.com".to_string() }

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct GuardrailConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub guardrail_type: String,
    /// If true, a guardrail error causes Allow rather than Block.
    #[serde(default)]
    pub fail_open: bool,
    /// API key override (e.g. for openai_moderation). Falls back to providers.openai.api_key.
    #[serde(default)]
    pub api_key: Option<String>,
    /// HTTP endpoint for external guardrails (e.g. Presidio).
    #[serde(default)]
    pub endpoint: Option<String>,
}

#[cfg(feature = "otel")]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_otel_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_service_name")]
    pub service_name: String,
    #[serde(default = "default_sample_ratio")]
    pub sample_ratio: f64,
    #[serde(default = "default_slow_threshold_ms")]
    pub slow_threshold_ms: u64,
    #[serde(default = "default_batch_queue_size")]
    pub batch_queue_size: usize,
    #[serde(default = "default_batch_delay_ms")]
    pub batch_scheduled_delay_ms: u64,
    #[serde(default = "default_batch_export_size")]
    pub batch_max_export_size: usize,
}

#[cfg(feature = "otel")]
impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: default_otel_endpoint(),
            service_name: default_service_name(),
            sample_ratio: default_sample_ratio(),
            slow_threshold_ms: default_slow_threshold_ms(),
            batch_queue_size: default_batch_queue_size(),
            batch_scheduled_delay_ms: default_batch_delay_ms(),
            batch_max_export_size: default_batch_export_size(),
        }
    }
}

#[cfg(feature = "otel")]
fn default_otel_endpoint() -> String { "http://localhost:4317".to_string() }
#[cfg(feature = "otel")]
fn default_service_name() -> String { "modelrouter".to_string() }
#[cfg(feature = "otel")]
fn default_sample_ratio() -> f64 { 0.1 }
#[cfg(feature = "otel")]
fn default_slow_threshold_ms() -> u64 { 2000 }
#[cfg(feature = "otel")]
fn default_batch_queue_size() -> usize { 2048 }
#[cfg(feature = "otel")]
fn default_batch_delay_ms() -> u64 { 5000 }
#[cfg(feature = "otel")]
fn default_batch_export_size() -> usize { 512 }

/// Condition for a declarative policy rule. All provided fields must match.
/// An empty condition matches every request.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PolicyConditionConfig {
    /// Match on the user's API key tag.
    pub tag: Option<String>,
    /// Match on a specific user ID.
    pub user_id: Option<i64>,
    /// Match on the requested model string.
    pub model: Option<String>,
}

fn default_policy_window() -> String { "monthly".to_string() }

/// A single declarative policy rule.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolicyRuleConfig {
    /// Human-readable name (logged on match, not used for uniqueness checks).
    pub name: String,
    /// Conditions that must ALL match for this rule to apply.
    #[serde(default)]
    pub condition: PolicyConditionConfig,
    /// Allowlist of model strings. If non-empty, any model not in the list is denied 403.
    #[serde(default)]
    pub allow_models: Vec<String>,
    /// USD spend limit for the window. None = no budget cap.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    /// Budget window: "daily", "weekly", or "monthly".
    #[serde(default = "default_policy_window")]
    pub window: String,
    /// Sort order — higher priority rules are evaluated first. Default 0.
    #[serde(default)]
    pub priority: i32,
}

#[cfg(test)]
mod shortcuts_tests {
    use super::*;

    #[test]
    fn shortcuts_parse() {
        let s: Settings = toml::from_str(r#"
            [routing.shortcuts]
            fastest  = "anthropic/claude-haiku-4-5"
            cheapest = "deepseek/deepseek-chat"
        "#).unwrap();
        assert_eq!(s.routing.shortcuts.fastest.as_deref(), Some("anthropic/claude-haiku-4-5"));
        assert_eq!(s.routing.shortcuts.cheapest.as_deref(), Some("deepseek/deepseek-chat"));
    }

    #[test]
    fn shortcuts_default_is_none() {
        let s: Settings = toml::from_str("").unwrap();
        assert!(s.routing.shortcuts.fastest.is_none());
        assert!(s.routing.shortcuts.cheapest.is_none());
    }
}

#[cfg(test)]
mod policy_rule_tests {
    use super::*;

    #[test]
    fn policy_rule_defaults() {
        let rule: PolicyRuleConfig = toml::from_str(r#"
            name = "test"
        "#).unwrap();
        assert_eq!(rule.name, "test");
        assert_eq!(rule.priority, 0);
        assert_eq!(rule.window, "monthly");
        assert!(rule.allow_models.is_empty());
        assert!(rule.budget_usd.is_none());
        assert!(rule.condition.tag.is_none());
    }

    #[test]
    fn policy_rule_full_parse() {
        let rule: PolicyRuleConfig = toml::from_str(r#"
            name = "research-team-opus"
            priority = 10
            allow_models = ["claude-opus-4-5"]
            budget_usd = 200.0
            window = "monthly"
            [condition]
            tag = "research"
        "#).unwrap();
        assert_eq!(rule.priority, 10);
        assert_eq!(rule.condition.tag.as_deref(), Some("research"));
        assert_eq!(rule.budget_usd, Some(200.0));
    }

    #[test]
    fn settings_policy_rules_field() {
        let s: Settings = toml::from_str(r#"
            [[policy_rules]]
            name = "allow-all"
        "#).unwrap();
        assert_eq!(s.policy_rules.len(), 1);
    }
}
