pub mod commands;

use anyhow::Result;
use commands::{Cli, Commands};

const CONFIG_TEMPLATE: &str = include_str!("../../config.example.toml");

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Init => {
            let config_dir = dirs::home_dir()
                .unwrap_or_default()
                .join(".modelrouter");
            tokio::fs::create_dir_all(&config_dir).await?;
            let config_path = config_dir.join("config.toml");
            if config_path.exists() {
                println!("Config already exists at {}", config_path.display());
            } else {
                tokio::fs::write(&config_path, CONFIG_TEMPLATE).await?;
                println!("Created config at {}", config_path.display());
            }
        }
        Commands::Serve { host, port } => {
            println!("serve {host}:{port} — not yet implemented");
        }
        Commands::Migrate => {
            println!("migrate — not yet implemented");
        }
        Commands::User(_) => {
            println!("user — not yet implemented");
        }
        Commands::Budget(_) => {
            println!("budget — not yet implemented");
        }
        Commands::Report(_) => {
            println!("report — not yet implemented");
        }
        Commands::Audit { .. } => {
            println!("audit — not yet implemented");
        }
        Commands::InstallService => {
            println!("install-service — not yet implemented");
        }
        Commands::UninstallService => {
            println!("uninstall-service — not yet implemented");
        }
    }
    Ok(())
}
