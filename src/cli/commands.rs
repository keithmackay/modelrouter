use clap::{Args, Parser, Subcommand};
use std::fmt;
use std::path::PathBuf;
use crate::report::formatter::OutputFormat;

#[derive(Parser)]
#[command(name = "modelrouter", version, about = "Self-hosted LLM proxy with budget controls")]
pub struct Cli {
    #[arg(long, global = true, env = "MODELROUTER_CONFIG")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialise config file and database
    Init,
    /// Start the proxy server
    Serve {
        /// Address to bind. Overrides `[server] host` in config.toml.
        ///
        /// No `default_value`: a clap default would always win over the
        /// config file, which is exactly how `[server]` was ignored (#55).
        #[arg(long)]
        host: Option<String>,
        /// Port to bind. Overrides `[server] port` in config.toml.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Run database migrations
    Migrate,
    /// Manage proxy users
    User(UserArgs),
    /// Manage budget rules
    Budget(BudgetArgs),
    /// Manage groups
    Group(GroupArgs),
    /// Generate reports
    Report(ReportArgs),
    /// View audit log
    Audit {
        #[arg(long, default_value_t = 50)]
        tail: u32,
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
    /// Install system service
    InstallService,
    /// Uninstall system service
    UninstallService,
    /// Manage admin users
    Admin(AdminArgs),
    /// Manage API keys
    Key(KeyArgs),
    /// Test TLS connectivity to each configured provider
    CheckTls,
    /// Manage models and failover chains
    Model(ModelArgs),
    /// Manage runtime model aliases
    Alias(AliasArgs),
    /// Enable or disable whole providers
    Provider(ProviderArgs),
    /// Manage outbound webhook callbacks
    Webhook(WebhookArgs),
    /// Inspect and control the response cache on a running router
    Cache(CacheArgs),
    /// Manage controlled experiments (spec §7a)
    Experiment(ExperimentArgs),
}

// ── Experiment subcommands ────────────────────────────────────────────────────

/// Experiments are rows in the database, so these commands write it
/// directly, the way `alias` and `webhook` do. A running server reloads its
/// experiment registry every 60 seconds, so an experiment added or closed
/// here is honoured within a minute without a restart.
#[derive(Args)]
pub struct ExperimentArgs {
    #[command(subcommand)]
    pub command: ExperimentCommands,
}

#[derive(Subcommand)]
pub enum ExperimentCommands {
    /// Create an experiment. A running server picks it up within 60 seconds.
    Add(ExperimentAddArgs),
    /// List experiments (active by default)
    List {
        /// Which experiments to show: active | closed | all
        #[arg(long, default_value = "active")]
        status: String,
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
    /// Close an experiment. A running server stops binding to it within 60 seconds.
    Close {
        #[arg(long)]
        id: i64,
    },
    /// Results for one experiment (the same document as
    /// `GET /admin/api/experiments/:id/results`)
    Results {
        #[arg(long)]
        id: i64,
        /// Runs per page (1-1000, default 200)
        #[arg(long)]
        limit: Option<i64>,
        /// Runs to skip
        #[arg(long)]
        offset: Option<i64>,
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
}

/// Flags of `experiment add`. Expiry and retention are required with no
/// default, so an operator always states them (spec §7a).
#[derive(Args, Debug)]
pub struct ExperimentAddArgs {
    /// Experiment name (unique, 1-128 characters)
    #[arg(long)]
    pub name: String,
    /// One variant as `LABEL=KEY:TARGET[,KEY:TARGET...]`, where KEY is the
    /// model name a caller requests and TARGET is an alias or
    /// `provider/model` it is sent to instead. Repeat for each variant (at
    /// least two). An empty overlay is `--variant control=`.
    #[arg(long = "variant", required = true, value_name = "LABEL=KEY:TARGET[,...]")]
    pub variants: Vec<String>,
    /// When the experiment stops binding requests: an RFC3339 timestamp in
    /// the future, or `never`
    #[arg(long, required = true, value_name = "RFC3339|never")]
    pub expires_at: String,
    /// Days after close that retained content is kept (0 = forever, at most 3650)
    #[arg(long, required = true, value_name = "DAYS")]
    pub content_retention_days: i64,
    /// Store full prompts and responses for bound requests regardless of
    /// `[storage]`; requires a finite --expires-at
    #[arg(long)]
    pub retain_content: bool,
    /// Mark the experiment's outcomes as input for later learning work
    #[arg(long)]
    pub feed_learning: bool,
    /// Restrict binding to these users (by name); repeat for each. Omit to
    /// admit every key.
    #[arg(long = "allow-user", value_name = "NAME")]
    pub allow_users: Vec<String>,
}

// ── Response cache subcommands ────────────────────────────────────────────────

/// The cache lives in the running server's process, so these commands talk to
/// the admin REST API rather than to the database directly.
#[derive(Args)]
pub struct CacheArgs {
    /// Base URL of the running router
    #[arg(long, env = "MODELROUTER_URL", default_value = "http://127.0.0.1:8080", global = true)]
    pub url: String,
    /// Admin JWT. If omitted, `--admin` + password prompt is used to log in.
    #[arg(long, env = "MODELROUTER_ADMIN_TOKEN", global = true)]
    pub token: Option<String>,
    /// Admin username to log in as when no token is supplied
    #[arg(long, global = true)]
    pub admin: Option<String>,
    #[command(subcommand)]
    pub command: CacheCommands,
}

#[derive(Subcommand)]
pub enum CacheCommands {
    /// Show hit rate, size, evictions and top cached models
    Stats {
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
    /// Purge cached responses
    Purge {
        /// Purge everything (default when neither --model nor --key is given)
        #[arg(long)]
        all: bool,
        /// Purge every entry for one model
        #[arg(long)]
        model: Option<String>,
        /// Purge one exact cache key
        #[arg(long)]
        key: Option<String>,
    },
    /// Read or change the eligibility policy
    Policy(CachePolicyArgs),
}

#[derive(Args)]
pub struct CachePolicyArgs {
    #[command(subcommand)]
    pub command: CachePolicyCommands,
}

#[derive(Subcommand)]
pub enum CachePolicyCommands {
    /// Print the live policy
    Get,
    /// Change the live policy (runtime only — the config file is not rewritten)
    Set {
        #[arg(long)]
        enabled: Option<bool>,
        #[arg(long)]
        completions_enabled: Option<bool>,
        /// Cache completions only at or below this temperature
        #[arg(long)]
        max_temperature: Option<f64>,
        #[arg(long)]
        completions_ttl_seconds: Option<u64>,
        #[arg(long)]
        search_enabled: Option<bool>,
        #[arg(long)]
        search_ttl_seconds: Option<u64>,
    },
}

#[derive(Args)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub command: ProviderCommands,
}

#[derive(Subcommand)]
pub enum ProviderCommands {
    /// List configured providers and their enable state
    List,
    /// Take a whole provider out of rotation. Sticky: stays disabled until re-enabled.
    Disable {
        /// Provider name, e.g. "anthropic"
        provider: String,
        /// Why it is being disabled — shown to callers and recorded in the audit log
        #[arg(long)]
        reason: Option<String>,
    },
    /// Re-enable a provider, clearing the recorded disable reason
    Enable {
        provider: String,
    },
}

#[derive(Args)]
pub struct AliasArgs {
    #[command(subcommand)]
    pub command: AliasCommands,
}

#[derive(Subcommand)]
pub enum AliasCommands {
    /// List runtime aliases and the effective alias map
    List {
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
    /// Create or update an alias (replaces the target if it exists)
    Set {
        /// Alias callers will request, e.g. "deep"
        alias: String,
        /// Target model, e.g. "anthropic/claude-opus-4-6" or another alias
        target: String,
    },
    /// Remove an alias
    Rm {
        alias: String,
    },
}

#[derive(Args)]
pub struct ModelArgs {
    #[command(subcommand)]
    pub command: ModelCommands,
}

#[derive(Subcommand)]
pub enum ModelCommands {
    /// Register a new model
    Add {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        name: String,
        /// Optional short alias (e.g. "opus")
        #[arg(long)]
        alias: Option<String>,
    },
    /// List all registered models
    List,
    /// Re-enable a model by ID, clearing the recorded disable reason
    Enable {
        #[arg(long)]
        id: i64,
    },
    /// Take a model out of rotation by ID. Sticky: stays disabled until re-enabled.
    Disable {
        #[arg(long)]
        id: i64,
        /// Why it is being disabled — shown to callers and recorded in the audit log
        #[arg(long)]
        reason: Option<String>,
    },
    /// Delete a model by ID
    Delete {
        #[arg(long)]
        id: i64,
    },
    /// Manage failover chains
    Failover(FailoverArgs),
}

#[derive(Args)]
pub struct FailoverArgs {
    #[command(subcommand)]
    pub command: FailoverCommands,
}

#[derive(Subcommand)]
pub enum FailoverCommands {
    /// Set ordered failover chain for a model (replaces existing)
    Set {
        /// Primary model (alias or provider/name)
        #[arg(long)]
        model: String,
        /// Comma-separated ordered fallback models
        #[arg(long)]
        fallback: String,
    },
    /// List failover chains
    List {
        /// Filter to a specific primary model
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Args)]
pub struct UserArgs {
    #[command(subcommand)]
    pub command: UserCommands,
}

#[derive(Subcommand)]
pub enum UserCommands {
    /// Create a new user
    Create {
        #[arg(long)]
        name: String,
    },
    /// List all users
    List,
    /// Enable or disable a user
    Enable { name: String },
    Disable { name: String },
    /// Rotate a user's API key
    RotateKey { name: String },
}

#[derive(Args)]
pub struct BudgetArgs {
    #[command(subcommand)]
    pub command: BudgetCommands,
}

#[derive(Subcommand)]
pub enum BudgetCommands {
    /// Set a budget rule (exactly one scope flag required)
    Set {
        // Scope flags (exactly one required)
        /// Org-wide rule
        #[arg(long)]
        global: bool,
        /// Project-scoped rule
        #[arg(long)]
        project: Option<String>,
        /// User-scoped rule (by name)
        #[arg(long)]
        user: Option<String>,
        /// Group soft target
        #[arg(long)]
        group: Option<String>,

        // Window
        #[arg(long, default_value = "monthly")]
        window: String,
        #[arg(long)]
        window_start: Option<String>,
        #[arg(long)]
        window_end: Option<String>,

        // Limits
        #[arg(long)]
        limit_usd: Option<f64>,
        #[arg(long)]
        limit_tokens: Option<i64>,
        #[arg(long)]
        rate_rpm: Option<i64>,
        #[arg(long)]
        max_concurrent: Option<i64>,
        /// Comma-separated model names to allow
        #[arg(long)]
        model_allow: Option<String>,
        /// Comma-separated model names to deny
        #[arg(long)]
        model_deny: Option<String>,
    },
    /// Edit an existing budget rule by ID
    Edit {
        #[arg(long)]
        id: i64,
        #[arg(long)]
        limit_usd: Option<f64>,
        #[arg(long)]
        limit_tokens: Option<i64>,
        #[arg(long)]
        rate_rpm: Option<i64>,
        #[arg(long)]
        max_concurrent: Option<i64>,
        #[arg(long)]
        model_allow: Option<String>,
        #[arg(long)]
        model_deny: Option<String>,
        #[arg(long)]
        window_start: Option<String>,
        #[arg(long)]
        window_end: Option<String>,
    },
    /// Delete a budget rule by ID
    Delete {
        #[arg(long)]
        id: i64,
    },
    /// List budget rules
    List {
        #[arg(long)]
        user: Option<String>,
    },
}

// ── Group subcommands ─────────────────────────────────────────────────────────

#[derive(Args)]
pub struct GroupArgs {
    #[command(subcommand)]
    pub command: GroupCommands,
}

#[derive(Subcommand)]
pub enum GroupCommands {
    /// Create a new group
    Create {
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 0)]
        priority: i64,
    },
    /// List all groups
    List,
    /// Enable a group
    Enable { name: String },
    /// Disable a group
    Disable { name: String },
    /// List members of a group
    Members {
        #[arg(long)]
        group: String,
    },
    /// Add a user to a group
    AddMember {
        #[arg(long)]
        group: String,
        #[arg(long)]
        user: String,
    },
    /// Remove a user from a group
    RemoveMember {
        #[arg(long)]
        group: String,
        #[arg(long)]
        user: String,
    },
}

/// Breakdown dimension for an attribution cost report.
#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum AttributionBreakdown {
    /// One row per model, plus a totals row
    Model,
    /// One row per calendar day, plus a totals row
    Day,
}

#[derive(Args)]
pub struct ReportArgs {
    #[command(subcommand)]
    pub command: ReportCommands,
}

#[derive(Subcommand)]
pub enum ReportCommands {
    /// Cost report
    Cost {
        /// Filter by user name
        #[arg(long)]
        user: Option<String>,
        /// Filter by group name (shows only active members of the group)
        #[arg(long)]
        group: Option<String>,
        /// Filter by project
        #[arg(long)]
        project: Option<String>,
        /// Filter by model name
        #[arg(long)]
        model: Option<String>,
        /// Filter by API key ID
        #[arg(long)]
        key_id: Option<i64>,
        /// Report on one caller-supplied attribution tag, as `key=value`
        /// (e.g. `--tag engagement=eng-4711`). Reports spend *and* cache
        /// savings for that unit of work; other filters do not apply.
        #[arg(long, value_name = "KEY=VALUE")]
        tag: Option<String>,
        /// Report on one caller-supplied correlation id. Mutually exclusive
        /// with --tag.
        #[arg(long, conflicts_with = "tag")]
        correlation_id: Option<String>,
        /// Break the attribution report down by `model` or `day`
        #[arg(long, default_value = "model")]
        by: AttributionBreakdown,
        /// Time window: daily | weekly | monthly | alltime  [default: monthly]
        #[arg(long, default_value = "monthly")]
        window: String,
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
    /// Compare two experiment arms side by side (same query as
    /// `GET /admin/api/compare` and the dashboard's Compare page)
    Compare {
        /// Partition traffic by `model`, `provider`, `tag` or `run`
        /// (attribution correlation id)
        #[arg(long)]
        dimension: String,
        /// Attribution tag key; required with `--dimension tag`
        #[arg(long)]
        key: Option<String>,
        /// Arm A value (a model, provider, tag value or correlation id)
        #[arg(long)]
        a: String,
        /// Arm B value
        #[arg(long)]
        b: String,
        /// Time window: daily | weekly | monthly | alltime  [default: monthly]
        #[arg(long, default_value = "monthly")]
        window: String,
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
    /// Usage report with flexible scope/window/granularity
    Usage {
        // Granularity (exactly one required)
        /// One aggregate row
        #[arg(long)]
        total: bool,
        /// Broken out by natural sub-unit
        #[arg(long)]
        subtotal: bool,
        /// Individual transactions + subtotals + totals
        #[arg(long)]
        detail: bool,

        // Window (exactly one required)
        /// No time bucketing
        #[arg(long)]
        alltime: bool,
        /// Bucket per year
        #[arg(long)]
        annual: bool,
        /// Bucket per month
        #[arg(long)]
        monthly: bool,

        // Scope (at least one required)
        /// All spend across all users
        #[arg(long)]
        global: bool,
        /// All spend in a group
        #[arg(long)]
        group: Option<String>,
        /// All spend on a project
        #[arg(long)]
        project: Option<String>,
        /// A specific user's spend
        #[arg(long)]
        user: Option<String>,

        // Output format
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
    /// Prompts report
    Prompts {
        #[arg(long)]
        user: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
    /// Audit log report
    Audit {
        #[arg(long)]
        actor: Option<String>,
        #[arg(long, default_value_t = 50)]
        tail: u32,
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
    /// Hooks performance report
    Hooks {
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
}

// ── Key subcommands ──────────────────────────────────────────────────────────

#[derive(Args)]
pub struct KeyArgs {
    #[command(subcommand)]
    pub command: KeyCommands,
}

#[derive(Subcommand)]
pub enum KeyCommands {
    /// Create a new API key for a user+project
    Create {
        #[arg(long)]
        user: String,
        #[arg(long)]
        project: String,
        #[arg(long)]
        label: Option<String>,
        /// Email address (reserved for future use — key will be printed to stdout)
        #[arg(long)]
        email: Option<String>,
        /// Synthetic session ID window in seconds (default: 28800 = 8 hours).
        /// Used by request.pre hooks to bucket opaque clients like Claude Code.
        #[arg(long)]
        session_window: Option<i64>,
    },
    /// List API keys
    List {
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        project: Option<String>,
    },
    /// Rotate the active key for a user+project (disables current, creates new)
    Rotate {
        #[arg(long)]
        user: String,
        #[arg(long)]
        project: String,
    },
    /// Disable the active key for a user+project
    Disable {
        #[arg(long)]
        user: String,
        #[arg(long)]
        project: String,
    },
}

// ── Admin subcommands ────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum AdminRole {
    Superadmin,
    Viewer,
}

impl fmt::Display for AdminRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdminRole::Superadmin => write!(f, "superadmin"),
            AdminRole::Viewer => write!(f, "viewer"),
        }
    }
}

impl std::str::FromStr for AdminRole {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "superadmin" => Ok(AdminRole::Superadmin),
            "viewer" => Ok(AdminRole::Viewer),
            other => Err(format!("role must be 'superadmin' or 'viewer', got '{}'", other)),
        }
    }
}

#[derive(Args)]
pub struct AdminArgs {
    #[command(subcommand)]
    pub command: AdminCommands,
}

#[derive(Subcommand)]
pub enum AdminCommands {
    /// Create a new admin user (prompts for password)
    Create {
        #[arg(long)]
        name: String,
        /// Role to assign. Default: superadmin.
        #[arg(long, default_value = "superadmin")]
        role: AdminRole,
    },
    /// List all admin users
    List {
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
    /// Reset an admin user's password (prompts for new password)
    ResetPassword {
        #[arg(long)]
        name: String,
    },
    /// Enable an admin user
    Enable {
        name: String,
    },
    /// Disable an admin user
    Disable {
        name: String,
    },
}

// ── Webhook subcommands ───────────────────────────────────────────────────────

#[derive(Args)]
pub struct WebhookArgs {
    #[command(subcommand)]
    pub command: WebhookCommands,
}

#[derive(Subcommand)]
pub enum WebhookCommands {
    /// List all configured webhooks
    List,
    /// Add a new webhook
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        url: String,
        #[arg(long, default_value = "completion")]
        events: String,
        #[arg(long)]
        secret_header_name: Option<String>,
        #[arg(long)]
        secret_header_value: Option<String>,
    },
    /// Delete a webhook by ID
    Delete {
        #[arg(long)]
        id: i64,
    },
    /// Enable a webhook by ID
    Enable {
        #[arg(long)]
        id: i64,
    },
    /// Disable a webhook by ID
    Disable {
        #[arg(long)]
        id: i64,
    },
}

#[cfg(test)]
mod tests {
    use super::AdminRole;
    use std::str::FromStr;

    #[test]
    fn admin_role_superadmin_parses() {
        let r = AdminRole::from_str("superadmin").unwrap();
        assert!(matches!(r, AdminRole::Superadmin));
        assert_eq!(r.to_string(), "superadmin");
    }

    #[test]
    fn admin_role_viewer_parses() {
        let r = AdminRole::from_str("viewer").unwrap();
        assert!(matches!(r, AdminRole::Viewer));
        assert_eq!(r.to_string(), "viewer");
    }

    #[test]
    fn admin_role_invalid_rejected() {
        let err = AdminRole::from_str("god").unwrap_err();
        assert!(err.contains("superadmin") && err.contains("viewer"));
    }
}
