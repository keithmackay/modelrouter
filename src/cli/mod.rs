pub mod commands;

use std::sync::Arc;

use anyhow::Result;
use commands::{Cli, Commands, UserCommands, BudgetCommands};

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

            // Sync hook permissions from config into DB
            crate::hooks::permissions::sync_hook_permissions(&db, &settings.hooks).await?;

            // Build app components
            let router =
                Arc::new(crate::router::engine::RequestRouter::new(settings.clone()));
            let cost_calc = Arc::new(crate::router::cost::CostCalculator::new());
            let provider_registry = Arc::new(
                crate::providers::registry::ProviderRegistry::new(
                    settings.providers.clone(),
                ),
            );
            let policy = Arc::new(crate::router::policy::PolicyEngine::new(db.clone()));

            let state = crate::api::app::AppState {
                settings: settings.clone(),
                db,
                router,
                cost_calc,
                provider_registry,
                policy,
            };
            let app = crate::api::app::build_router(state);

            let bind_addr = format!("{}:{}", host, port);
            tracing::info!("Listening on {}", bind_addr);
            let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
            axum::serve(listener, app).await?;
        }
        Commands::Migrate => {
            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;
            println!("Migrations complete.");
        }
        Commands::User(user_args) => {
            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;

            match user_args.command {
                UserCommands::Create { name, group } => {
                    use crate::db::repositories::users::UserRepository;
                    use crate::db::models::NewUser;
                    use crate::api::auth::hash_token;

                    let raw_token = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
                    let hash = hash_token(&raw_token);
                    let user = UserRepository::create(&db, NewUser {
                        name: name.clone(),
                        api_key_hash: hash,
                        group_name: group,
                    }).await?;
                    println!("Created user '{}' (id={})", user.name, user.id);
                    println!("API key: {}", raw_token);
                    println!("Store this key securely — it cannot be retrieved later.");
                }
                UserCommands::List => {
                    use crate::db::repositories::users::UserRepository;
                    let users = UserRepository::list(&db).await?;
                    for u in users {
                        println!(
                            "{:>4}  {:20}  {}  {}",
                            u.id,
                            u.name,
                            if u.enabled { "enabled" } else { "disabled" },
                            u.group_name.as_deref().unwrap_or("-")
                        );
                    }
                }
                UserCommands::Enable { name } => {
                    use crate::db::repositories::users::UserRepository;
                    let user = UserRepository::find_by_name(&db, &name).await?
                        .ok_or_else(|| anyhow::anyhow!("User not found: {}", name))?;
                    UserRepository::set_enabled(&db, user.id, true).await?;
                    println!("Enabled user '{}'", name);
                }
                UserCommands::Disable { name } => {
                    use crate::db::repositories::users::UserRepository;
                    let user = UserRepository::find_by_name(&db, &name).await?
                        .ok_or_else(|| anyhow::anyhow!("User not found: {}", name))?;
                    UserRepository::set_enabled(&db, user.id, false).await?;
                    println!("Disabled user '{}'", name);
                }
                UserCommands::RotateKey { name } => {
                    use crate::db::repositories::users::UserRepository;
                    use crate::api::auth::hash_token;
                    let user = UserRepository::find_by_name(&db, &name).await?
                        .ok_or_else(|| anyhow::anyhow!("User not found: {}", name))?;
                    let new_token = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
                    let new_hash = hash_token(&new_token);
                    let overlap_expires_at = (chrono::Utc::now()
                        + chrono::Duration::minutes(settings.auth.rotation_overlap_mins))
                        .to_rfc3339();
                    UserRepository::rotate_key(&db, user.id, &new_hash, &overlap_expires_at).await?;
                    println!("Rotated key for user '{}'", name);
                    println!("New API key: {}", new_token);
                    println!("Old key valid until: {}", overlap_expires_at);
                }
            }
        }
        Commands::Budget(budget_args) => {
            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;

            match budget_args.command {
                BudgetCommands::Set { user, window, limit_usd } => {
                    use crate::db::repositories::{users::UserRepository, budgets::BudgetRepository};
                    use crate::db::models::NewBudgetRule;
                    let found = UserRepository::find_by_name(&db, &user).await?
                        .ok_or_else(|| anyhow::anyhow!("User not found: {}", user))?;
                    let rule = BudgetRepository::create(&db, NewBudgetRule {
                        user_id: Some(found.id),
                        group_name: None,
                        window: window.clone(),
                        limit_usd,
                        limit_tokens: None,
                        rate_rpm: None,
                        model_allow: vec![],
                        model_deny: vec![],
                    }).await?;
                    println!("Created budget rule (id={}) for user '{}': {} window, limit=${:?}", rule.id, user, window, limit_usd);
                }
                BudgetCommands::List { user } => {
                    use crate::db::repositories::{users::UserRepository, budgets::BudgetRepository};
                    if let Some(name) = user {
                        let found = UserRepository::find_by_name(&db, &name).await?
                            .ok_or_else(|| anyhow::anyhow!("User not found: {}", name))?;
                        let rules = BudgetRepository::list_for_user(&db, found.id).await?;
                        for r in rules {
                            println!("{:>4}  user_id={:?}  window={}  limit_usd={:?}  rate_rpm={:?}", r.id, r.user_id, r.window, r.limit_usd, r.rate_rpm);
                        }
                    } else {
                        let users = UserRepository::list(&db).await?;
                        for u in &users {
                            let rules = BudgetRepository::list_for_user(&db, u.id).await?;
                            for r in rules {
                                println!("{:>4}  user={}  window={}  limit_usd={:?}  rate_rpm={:?}", r.id, u.name, r.window, r.limit_usd, r.rate_rpm);
                            }
                        }
                    }
                }
            }
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
