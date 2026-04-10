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
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 8080)]
        port: u16,
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
        /// Filter by project (matches api_keys.project assigned at key creation)
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value = "monthly")]
        window: String,
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
    /// Usage report
    Usage {
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        since: Option<String>,
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
