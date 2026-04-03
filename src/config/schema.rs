// src/config/schema.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PricingEntry {
    pub model: String,
    pub input_per_million: f64,
    pub output_per_million: f64,
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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            request_body_limit_mb: default_request_body_limit_mb(),
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
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            default_provider: default_provider(),
            default_model: default_model(),
            model_aliases: HashMap::new(),
            fallback_chains: HashMap::new(),
        }
    }
}

fn default_provider() -> String { "openai".to_string() }
fn default_model() -> String { "gpt-4o".to_string() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_key: String,
    pub api_base: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
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
