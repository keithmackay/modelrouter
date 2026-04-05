// src/config/schema.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PricingEntry {
    pub model: String,
    pub input_per_million: f64,
    pub output_per_million: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CacheConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_cache_max_entries")]
    pub max_entries: u64,
    #[serde(default = "default_cache_ttl")]
    pub ttl_seconds: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_entries: default_cache_max_entries(),
            ttl_seconds: default_cache_ttl(),
        }
    }
}

fn default_cache_max_entries() -> u64 { 1000 }
fn default_cache_ttl() -> u64 { 3600 }

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
    #[cfg(feature = "otel")]
    #[serde(default)]
    pub telemetry: TelemetryConfig,
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
