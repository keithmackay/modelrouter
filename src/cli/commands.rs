use clap::{Args, Parser, Subcommand};
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
        #[arg(long)]
        group: Option<String>,
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
    /// Set a budget rule
    Set {
        #[arg(long)]
        user: String,
        #[arg(long)]
        window: String, // daily|weekly|monthly
        #[arg(long)]
        limit_usd: Option<f64>,
    },
    /// List budget rules
    List {
        #[arg(long)]
        user: Option<String>,
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
        #[arg(long, conflicts_with = "group")]
        user: Option<String>,
        /// Filter by group name
        #[arg(long, conflicts_with = "user")]
        group: Option<String>,
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
