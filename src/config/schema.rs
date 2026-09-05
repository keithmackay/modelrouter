// src/config/schema.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Admin role vocabulary. The complete set of recognized roles for admin users.
///
/// Session extractors gate on `role != "superadmin"`, so an unknown role would
/// silently degrade to viewer. Both OIDC auto-provisioning and bootstrap config
/// validation check against this list to ensure operators set explicit valid roles.
pub const ADMIN_ROLE_VOCABULARY: &[&str] = &["superadmin", "viewer"];

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

/// Per-model declaration of which sampling parameters the model accepts.
///
/// Providers drop parameters between model versions — Anthropic's Claude 5
/// family rejects `temperature` with a 400 while `claude-haiku-4-5`, behind the
/// same provider, still honours it. The router carries a built-in table (see
/// [`crate::router::model_capabilities`]) and these entries override it, so an
/// operator can react to a provider change without waiting on a release.
///
/// `model` is matched with the provider prefix stripped and case-insensitively,
/// the same normalization [`PricingEntry`] uses.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ModelCapabilityEntry {
    pub model: String,
    /// `false` makes the router strip `temperature` before dispatch. Omitted
    /// leaves the built-in default in force.
    #[serde(default)]
    pub supports_temperature: Option<bool>,
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
    pub model_capabilities: Vec<ModelCapabilityEntry>,
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
    #[serde(default)]
    pub storage: StorageConfig,
    #[cfg(feature = "otel")]
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub archival: ArchivalConfig,
    #[serde(default)]
    pub oidc: OidcConfig,
    #[serde(default)]
    pub health: HealthConfig,
    #[serde(default)]
    pub admin: AdminConfig,
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

fn default_oidc_role() -> String { "viewer".to_string() }

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

impl OidcConfig {
    /// Validate the configured OIDC role against the known vocabulary.
    ///
    /// An unknown role would silently degrade to viewer in session extractors
    /// that gate on `role != "superadmin"`, so this validation rejects startup
    /// loudly rather than allowing misconfigured SSO admins to believe they hold
    /// the specified role when they do not.
    pub fn validate_role(&self) -> anyhow::Result<()> {
        if !ADMIN_ROLE_VOCABULARY.contains(&self.auto_provision_role.as_str()) {
            anyhow::bail!(
                "oidc.auto_provision_role is '{}', which is not a recognized role. \
                 Valid roles are: {}. Set the role explicitly in config.toml or \
                 via MODELROUTER_OIDC__AUTO_PROVISION_ROLE. Refusing to start.",
                self.auto_provision_role,
                ADMIN_ROLE_VOCABULARY.join(", ")
            );
        }
        Ok(())
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
    /// Search engine used when a `/v1/search` request omits `engine`.
    ///
    /// Unset means "infer from the configured search providers": if exactly one
    /// is configured, use it. This exists because the previous behaviour was a
    /// hardcoded `"tavily"` in `api/routes/search.rs`, which 502s on any host
    /// that configures a different engine — a caller omitting `engine` got
    /// `No search adapter configured for engine: tavily` even though a working
    /// Vertex adapter was configured and reachable. Naming a provider in code
    /// as the fallback for "caller said nothing" is the same class of mistake
    /// `strict_model_resolution` exists to prevent for chat models.
    #[serde(default)]
    pub default_search_engine: Option<String>,
    /// Fallback chains for search engines (engine → ordered list of fallback engines).
    /// Shape-consistent with `fallback_chains` for LLM routing. When a search request
    /// fails due to provider error (timeout, 5xx, rate-limit), the chain is walked.
    /// Caller errors (invalid query → 400) never trigger failover.
    #[serde(default)]
    pub search_fallback_chains: HashMap<String, Vec<String>>,
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
            default_search_engine: None,
            search_fallback_chains: HashMap::new(),
        }
    }
}

fn default_provider() -> String { "openai".to_string() }
fn default_model() -> String { "gpt-4o".to_string() }

/// `[health]` — deep-health probing (GET /health/deep).
///
/// The deep endpoint issues one MINIMAL real call per capability (completion,
/// embedding, search) through the normal routing path, so it proves routing
/// rules and provider credentials, not just process liveness. Probes cost real
/// provider money: the TTL caps how often polling can trigger them.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthConfig {
    /// Seconds a deep-health result is served from memory before re-probing.
    #[serde(default = "default_deep_ttl")]
    pub deep_ttl_seconds: u64,
    /// Model the LLM probe requests (resolved through aliases/routing exactly
    /// like a caller's request). Defaults to `routing.default_model`.
    #[serde(default)]
    pub llm_probe_model: Option<String>,
    /// Model the embedding probe requests.
    #[serde(default = "default_embedding_probe_model")]
    pub embedding_probe_model: String,
    /// Engine the search probe uses.
    #[serde(default = "default_search_probe_engine")]
    pub search_probe_engine: String,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            deep_ttl_seconds: default_deep_ttl(),
            llm_probe_model: None,
            embedding_probe_model: default_embedding_probe_model(),
            search_probe_engine: default_search_probe_engine(),
        }
    }
}

fn default_deep_ttl() -> u64 { 60 }
fn default_embedding_probe_model() -> String { "text-embedding-3-small".to_string() }
fn default_search_probe_engine() -> String { "tavily".to_string() }

/// `[admin]` — admin account management.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AdminConfig {
    #[serde(default)]
    pub bootstrap: Option<AdminBootstrapConfig>,
}

/// `[admin.bootstrap]` — idempotent admin account creation at startup (issue #43).
///
/// When present, the account is created-if-absent by name at serve time.
/// Second startup is a no-op: password and role are never overwritten. A
/// malformed bcrypt hash or invalid role fails startup loudly.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdminBootstrapConfig {
    pub name: String,
    pub role: String,
    /// Bcrypt hash. NEVER plaintext.
    pub password_hash: String,
}

impl AdminBootstrapConfig {
    /// Validate role against the known vocabulary and bcrypt hash format.
    ///
    /// An unknown role would silently degrade to viewer in session extractors
    /// that gate on `role != "superadmin"`, so this validation rejects startup
    /// loudly rather than allowing misconfigured bootstrap accounts to believe
    /// they hold the specified role when they do not.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !ADMIN_ROLE_VOCABULARY.contains(&self.role.as_str()) {
            anyhow::bail!(
                "admin.bootstrap.role is '{}', which is not a recognized role. \
                 Valid roles are: {}. Set the role explicitly in config.toml or \
                 via MODELROUTER_ADMIN__BOOTSTRAP__ROLE. Refusing to start.",
                self.role,
                ADMIN_ROLE_VOCABULARY.join(", ")
            );
        }

        // Validate bcrypt hash format: bcrypt hashes start with $2a$, $2b$, or $2y$
        // and have a specific structure. We attempt verification with a dummy password
        // to check if the hash is well-formed — any result (Ok/Err) proves it parses.
        if !self.password_hash.starts_with("$2") {
            anyhow::bail!(
                "admin.bootstrap.password_hash does not look like a bcrypt hash. \
                 Expected format: $2a$..., $2b$..., or $2y$... \
                 Use `modelrouter admin hash-password` to generate a valid hash. \
                 Refusing to start."
            );
        }

        // Validate hash is parseable by attempting verification.
        // Both Ok and Err are acceptable — we just need to ensure it doesn't panic.
        let _ = bcrypt::verify("_validation_probe_", &self.password_hash)
            .map_err(|_| anyhow::anyhow!(
                "admin.bootstrap.password_hash is malformed (bcrypt verification failed). \
                 Use `modelrouter admin hash-password` to generate a valid hash. \
                 Refusing to start."
            ))?;

        Ok(())
    }

    /// Apply the bootstrap admin account (issue #43).
    /// Idempotent: create-if-absent by name; second startup is a no-op.
    /// Invalid role or malformed hash fails loudly.
    pub async fn apply(
        &self,
        db: &dyn crate::api::app::DatabaseProvider,
    ) -> anyhow::Result<()> {
        self.validate()?;
        use crate::db::repositories::admin_users::AdminUserRepository;
        match AdminUserRepository::find_by_name(db, &self.name).await? {
            Some(existing) => {
                tracing::info!(
                    name = %existing.name,
                    role = %existing.role,
                    "admin bootstrap: account already exists, skipping"
                );
            }
            None => {
                let admin = AdminUserRepository::create(
                    db,
                    crate::db::models::NewAdminUser {
                        name: self.name.clone(),
                        password_hash: self.password_hash.clone(),
                        role: self.role.clone(),
                    },
                )
                .await?;
                tracing::info!(
                    name = %admin.name,
                    role = %admin.role,
                    "admin bootstrap: created account"
                );
            }
        }
        Ok(())
    }
}

/// `[storage]` — what the prompt log records (issue #4).
///
/// Two independent switches: whether prompt-log rows are written at all, and
/// whether those rows carry message/response content or metadata only.
/// Content storage defaults OFF because the log is operator telemetry —
/// privacy-sensitive deployments should not have to discover a config flag
/// after the fact to stop full conversation bodies landing on disk. Cost
/// tracking (`cost_ledger`) is unaffected by either switch.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    /// Write prompt-log rows at all. `false` skips the INSERT entirely.
    #[serde(default = "default_true")]
    pub store_prompts: bool,
    /// Store full request messages and response bodies. `false` (default)
    /// stores metadata only (tokens, cost, model, timestamps). Governs the
    /// callback egress (Langfuse/LangSmith/webhook) as well as the row.
    #[serde(default)]
    pub store_prompt_content: bool,
    /// Purge prompt-log rows older than N days. 0 (default) = keep forever —
    /// deletion is strictly opt-in so an upgrade can never silently discard
    /// existing logs.
    #[serde(default)]
    pub prompt_retention_days: u64,
    /// Optional separate SQLite file for the prompt log (issue #29), so
    /// operators can rotate/archive logs independently of the main DB.
    /// config.toml only — pool selection is restart-scoped, so this field is
    /// deliberately NOT exposed in the GUI form and is preserved unchanged
    /// when the GUI saves the live toggles. Ignored (with a warning) on the
    /// Postgres backend.
    #[serde(default)]
    pub prompt_db_path: Option<String>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            store_prompts: true,
            store_prompt_content: false,
            prompt_retention_days: 0,
            prompt_db_path: None,
        }
    }
}

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
    /// has to name an embedding region separately, exactly as the pilot
    /// application's own client does with `EMBEDDING_REGION` (default `us-central1`) alongside its
    /// chat region. Falls back to `region` when unset.
    #[serde(default)]
    pub embedding_region: Option<String>,
    /// Vertex Model-as-a-Service region (Mistral, Llama, DeepSeek, and the
    /// other Model Garden partners served via the OpenAI-compatible
    /// `endpoints/openapi/chat/completions` path).
    ///
    /// Exactly the embedding_region story again: MaaS models are regional-only
    /// — `locations/global` answers 404 for them even though Claude and Gemini
    /// chat models run there. Falls back to `region` when that is itself
    /// regional; with `region = "global"` and this unset, MaaS dispatch fails
    /// loudly with a fix-hint. No region name is baked into code (operator
    /// ruling 2026-08-20: specific regions/publishers are config, not code).
    #[serde(default)]
    pub maas_region: Option<String>,
    /// Vertex publisher catalogs to probe for the available-models endpoint
    /// (`google`, `anthropic`, `mistralai`, `meta`, `deepseek-ai`, `qwen`,
    /// `openai`, `ai21`, …). Operations data — which publishers a project can
    /// reach varies with Model Garden enablement, so the list lives here, not
    /// in code. Unset/empty = the structural floor (google + anthropic, the
    /// two with dedicated dispatch arms). Unreachable/refused publishers are
    /// skipped with a warning, never fatal.
    #[serde(default)]
    pub catalog_publishers: Option<Vec<String>>,
    /// Vertex embedding task type (`RETRIEVAL_DOCUMENT`, `RETRIEVAL_QUERY`,
    /// `SEMANTIC_SIMILARITY`, …), sent as each instance's `task_type`.
    ///
    /// It lives in config rather than in code because it has no equivalent in
    /// the OpenAI embeddings API the caller speaks, and because getting it wrong
    /// is silent: query-typed and document-typed vectors coexist happily in one
    /// store and simply retrieve worse. Omitted from the request when unset.
    #[serde(default)]
    pub embedding_task_type: Option<String>,
    /// Gemini model used to serve `/v1/search` for this provider.
    ///
    /// Web search on Vertex is grounding on a generative model, so unlike Tavily
    /// the engine has a model to choose. It is not addressed through
    /// `[routing.model_aliases]` because the search route resolves an ENGINE,
    /// not a model. Defaults to `gemini-2.5-flash` when unset.
    #[serde(default)]
    pub search_model: Option<String>,
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
            maas_region: None,
            catalog_publishers: None,
            embedding_task_type: None,
            search_model: None,
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

pub const PLACEHOLDER_JWT_SECRET: &str = "change-me-jwt-secret";

fn default_jwt_secret() -> String { PLACEHOLDER_JWT_SECRET.to_string() }

impl AuthConfig {
    /// `Err` when the signing key is unset or still the shipped placeholder.
    ///
    /// Every admin and dashboard session is authenticated by an HS256 signature
    /// over this value and nothing else, so an empty or well-known secret makes
    /// every session token forgeable. This is not hypothetical: the Helm chart
    /// shipped `jwt_secret = ""` in its ConfigMap on the assumption an env var
    /// overrode it, and a prefix typo in the env name meant it never did.
    pub fn validate_secret(&self) -> anyhow::Result<()> {
        if self.jwt_secret.trim().is_empty() {
            anyhow::bail!(
                "auth.jwt_secret is empty. Every admin session token is signed with it, so an empty \
                 value makes them all forgeable. Set it in config.toml or via \
                 MODELROUTER_AUTH__JWT_SECRET (note: one underscore after the prefix, two between \
                 path segments). Refusing to start."
            );
        }
        if self.jwt_secret == PLACEHOLDER_JWT_SECRET {
            anyhow::bail!(
                "auth.jwt_secret is still the shipped placeholder value. It is published in this \
                 repository, so anyone can forge an admin session. Set a real secret in config.toml \
                 or via MODELROUTER_AUTH__JWT_SECRET. Refusing to start."
            );
        }
        Ok(())
    }
}
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
    /// Response-cache participation for matched callers (issue #30).
    /// `Some(false)` opts them out — no cache lookup, no store — so a group
    /// doing creative/experimental work never gets replayed answers. `None`
    /// (default) inherits the global cache policy.
    #[serde(default)]
    pub cache: Option<bool>,
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
