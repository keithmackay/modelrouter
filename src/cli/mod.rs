pub mod commands;

use std::sync::Arc;

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
            let settings = crate::config::load(cli.config)?;
            let settings = Arc::new(settings);

            // Init DB
            let db =
                crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;
            let db: Arc<dyn crate::api::app::DatabaseProvider> = Arc::new(db);

            // Build app components
            let router =
                Arc::new(crate::router::engine::RequestRouter::new(settings.clone()));
            let cost_calc = Arc::new(crate::router::cost::CostCalculator::new());
            let provider_registry = Arc::new(
                crate::providers::registry::ProviderRegistry::new(
                    settings.providers.clone(),
                ),
            );

            let state = crate::api::app::AppState {
                settings: settings.clone(),
                db,
                router,
                cost_calc,
                provider_registry,
            };
            let app = crate::api::app::build_router(state);

            let bind_addr = format!("{}:{}", host, port);
            tracing::info!("Listening on {}", bind_addr);
            let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
            axum::serve(listener, app).await?;
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
