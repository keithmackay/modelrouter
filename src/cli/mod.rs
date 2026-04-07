pub mod commands;

use std::sync::Arc;

use anyhow::Result;
use commands::{Cli, Commands, UserCommands, BudgetCommands};
use crate::report::AuditRow;
use crate::report::formatter::{print_rows, OutputFormat};

// ── Service install/uninstall ─────────────────────────────────────────────────

#[cfg(target_os = "macos")]
const PLIST_CONTENT: &str = include_str!("../../contrib/dev.modelrouter.plist");

#[cfg(target_os = "linux")]
const SYSTEMD_CONTENT: &str = include_str!("../../contrib/modelrouter.service");

#[cfg(target_os = "macos")]
fn launchctl_uid() -> String {
    std::env::var("UID").unwrap_or_else(|_| {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "501".to_string())
    })
}

#[cfg(target_os = "macos")]
fn install_service() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let agents_dir = home.join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&agents_dir)?;
    let plist_path = agents_dir.join("dev.modelrouter.plist");
    std::fs::write(&plist_path, PLIST_CONTENT)?;
    println!("Installed plist to {}", plist_path.display());
    let path_str = plist_path.to_str()
        .ok_or_else(|| anyhow::anyhow!("Path contains non-UTF-8 characters: {}", plist_path.display()))?;
    let domain_target = format!("gui/{}", launchctl_uid());
    let status = std::process::Command::new("launchctl")
        .args(["bootstrap", &domain_target, path_str])
        .status()?;
    if status.success() {
        println!("Service bootstrapped via launchctl.");
    } else {
        anyhow::bail!("launchctl bootstrap failed (exit code: {})", status);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_service() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let plist_path = home.join("Library").join("LaunchAgents").join("dev.modelrouter.plist");
    if plist_path.exists() {
        let path_str = plist_path.to_str()
            .ok_or_else(|| anyhow::anyhow!("Path contains non-UTF-8 characters: {}", plist_path.display()))?;
        let domain_target = format!("gui/{}", launchctl_uid());
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &domain_target, path_str])
            .status();
        std::fs::remove_file(&plist_path)?;
        println!("Service booted out and plist removed.");
    } else {
        println!("No plist found at {}; nothing to do.", plist_path.display());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_service() -> Result<()> {
    let service_path = std::path::Path::new("/etc/systemd/system/modelrouter.service");
    std::fs::write(service_path, SYSTEMD_CONTENT)?;
    println!("Installed unit file to {}", service_path.display());
    let reload = std::process::Command::new("systemctl")
        .arg("daemon-reload")
        .status()?;
    if !reload.success() {
        anyhow::bail!("systemctl daemon-reload failed");
    }
    let enable = std::process::Command::new("systemctl")
        .args(["enable", "modelrouter"])
        .status()?;
    if enable.success() {
        println!("Service enabled. Run 'systemctl start modelrouter' to start it.");
    } else {
        anyhow::bail!("systemctl enable modelrouter failed");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_service() -> Result<()> {
    let _ = std::process::Command::new("systemctl")
        .args(["disable", "--now", "modelrouter"])
        .status();
    let service_path = std::path::Path::new("/etc/systemd/system/modelrouter.service");
    if service_path.exists() {
        std::fs::remove_file(service_path)?;
        let _ = std::process::Command::new("systemctl")
            .arg("daemon-reload")
            .status();
        println!("Service disabled and unit file removed.");
    } else {
        println!("No unit file found; nothing to do.");
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn install_service() -> Result<()> {
    anyhow::bail!("install-service is only supported on macOS and Linux");
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn uninstall_service() -> Result<()> {
    anyhow::bail!("uninstall-service is only supported on macOS and Linux");
}

fn print_audit_rows(rows: Vec<AuditRow>, fmt: OutputFormat) {
    print_rows(
        &rows,
        &["ID", "Actor", "Action", "Target", "Created At"],
        |r| {
            vec![
                r.id.to_string(),
                r.actor_name.clone(),
                r.action.clone(),
                r.target.clone().unwrap_or_default(),
                r.created_at.clone(),
            ]
        },
        fmt,
    );
}

const CONFIG_TEMPLATE: &str = include_str!("../../config.example.toml");

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Init => {
            println!("modelrouter v{}", env!("CARGO_PKG_VERSION"));
            println!();
            let config_dir = dirs::home_dir()
                .unwrap_or_default()
                .join(".modelrouter");
            tokio::fs::create_dir_all(&config_dir).await?;
            let config_path = config_dir.join("config.toml");
            if config_path.exists() {
                print!(
                    "Config already exists at {}. Overwrite? [y/N] ",
                    config_path.display()
                );
                use std::io::Write;
                std::io::stdout().flush()?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if input.trim().eq_ignore_ascii_case("y") {
                    tokio::fs::write(&config_path, CONFIG_TEMPLATE).await?;
                    println!("Overwrote config at {}", config_path.display());
                } else {
                    println!("Aborted.");
                    return Ok(());
                }
            } else {
                tokio::fs::write(&config_path, CONFIG_TEMPLATE).await?;
                println!("Created config at {}", config_path.display());
            }
            println!();
            println!("Next steps:");
            println!("  1. Edit {} to add your provider API keys", config_path.display());
            println!("  2. Run: modelrouter migrate");
            println!("  3. Run: modelrouter serve");
            println!("  4. Test: curl http://localhost:8080/health");
        }
        Commands::Serve { host, port } => {
            let config_path: Option<String> = cli.config.as_ref()
                .and_then(|p| p.to_str().map(|s| s.to_string()))
                .or_else(|| std::env::var("MODELROUTER_CONFIG").ok());
            let settings = crate::config::load(cli.config)?;
            let settings = Arc::new(settings);

            // Initialise tracing subscriber. The otel feature provides a richer layered
            // subscriber; without it we install a basic fmt subscriber.
            #[cfg(not(feature = "otel"))]
            {
                tracing_subscriber::fmt()
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::try_from_default_env()
                            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                    )
                    .try_init()
                    .ok();
            }
            #[cfg(feature = "otel")]
            let _telemetry_guard = {
                if settings.telemetry.enabled {
                    Some(crate::telemetry::init_telemetry(&settings.telemetry)?)
                } else {
                    tracing_subscriber::fmt()
                        .with_env_filter(
                            tracing_subscriber::EnvFilter::try_from_default_env()
                                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                        )
                        .try_init()
                        .ok();
                    None
                }
            };

            // Init DB
            let sqlite_db =
                crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&sqlite_db.pool).await?;
            let pool = sqlite_db.pool.clone();
            let db: Arc<dyn crate::api::app::DatabaseProvider> = Arc::new(sqlite_db);

            // Sync hook permissions from config into DB
            crate::hooks::permissions::sync_hook_permissions(&db, &settings.hooks).await?;

            // Build app components
            let router =
                Arc::new(crate::router::engine::RequestRouter::new(settings.clone()));
            let cost_calc = Arc::new(crate::router::cost::CostCalculator::new_with_config(&settings.pricing));
            let provider_registry = Arc::new(
                crate::providers::registry::ProviderRegistry::new(
                    settings.providers.clone(),
                ),
            );
            let fallback = Arc::new(crate::router::fallback::FallbackChain::new(
                settings.routing.fallback_chains.clone(),
            ));
            let complexity_router = Arc::new(crate::router::complexity::ComplexityRouter::new(
                settings.routing.complexity_routing.clone(),
            ));
            let response_cache = Arc::new(crate::router::cache::ResponseCache::new(&settings.cache));
            let embedding_registry = Arc::new(crate::providers::embed_registry::EmbeddingRegistry::new(
                settings.providers.clone(),
            ));
            let load_balancer = Arc::new(crate::router::load_balancer::LoadBalancer::new(
                settings.routing.load_balancer.clone(),
            ));

            #[cfg(feature = "prometheus")]
            let app_metrics = Some(Arc::new(
                crate::metrics::AppMetrics::new().expect("Failed to init Prometheus metrics")
            ));
            #[cfg(not(feature = "prometheus"))]
            let app_metrics: Option<std::convert::Infallible> = None;

            let live_settings = Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone()));

            let policy = Arc::new(
                crate::router::policy::PolicyEngine::new(db.clone())
                    .with_settings(live_settings.clone()),
            );

            let oidc_state = Arc::new(crate::api::admin::oidc::OidcStateStore::new());

            let state = crate::api::app::AppState {
                settings: settings.clone(),
                live_settings: live_settings.clone(),
                db,
                pool: Some(pool),
                router,
                cost_calc,
                provider_registry,
                policy,
                fallback,
                complexity_router,
                response_cache,
                embedding_registry,
                load_balancer,
                concurrency: Arc::new(crate::router::concurrency::ConcurrencyLimiter::new()),
                circuit_breaker: Arc::new(crate::router::circuit_breaker::CircuitBreaker::default()),
                ip_rate_limiter: Arc::new(crate::api::middleware::ip_rate_limit::IpRateLimiter::new(
                    settings.server.ip_rate_limit_rpm,
                )),
                session_limiter: Arc::new(crate::router::session_limits::SessionLimiter::new(
                    settings.session_limits.tpm,
                    settings.session_limits.rpm,
                )),
                callbacks: {
                    let mut backends: Vec<Box<dyn crate::callbacks::CallbackBackend>> = vec![];
                    if let Some(cfg) = settings.callbacks.langfuse.clone() {
                        backends.push(Box::new(crate::callbacks::langfuse::LangFuseBackend::new(cfg)));
                    }
                    if let Some(cfg) = settings.callbacks.langsmith.clone() {
                        backends.push(Box::new(crate::callbacks::langsmith::LangSmithBackend::new(cfg)));
                    }
                    Arc::new(crate::callbacks::CallbackDispatcher::new(backends))
                },
                guardrails: {
                    let mut chain: Vec<(Box<dyn crate::guardrails::Guardrail>, bool)> = vec![];
                    for cfg in &settings.guardrails {
                        match cfg.guardrail_type.as_str() {
                            "openai_moderation" => {
                                let api_key = cfg.api_key.clone()
                                    .or_else(|| settings.providers.get("openai").map(|p| p.api_key.clone()))
                                    .unwrap_or_default();
                                chain.push((
                                    Box::new(crate::guardrails::openai_moderation::OpenAIModerationGuardrail::with_fail_open(api_key, cfg.fail_open)),
                                    cfg.fail_open,
                                ));
                            }
                            other => tracing::warn!(guardrail_type = other, "Unknown guardrail type, skipping"),
                        }
                    }
                    Arc::new(crate::guardrails::GuardrailChain::new(chain))
                },
                app_metrics,
                oidc_state,
            };
            #[cfg(feature = "s3-archival")]
            if settings.archival.enabled {
                let job = crate::archival::ArchivalJob::new(
                    settings.archival.clone(),
                    state.db.clone(),
                );
                crate::archival::spawn_archival_task(job);
            }

            if let Some(ref cfg_path) = config_path {
                let loader = crate::config::loader::SettingsLoader::new(cfg_path.clone());
                let live = live_settings.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                        match loader.load() {
                            Ok(new_settings) => {
                                live.store(Arc::new(new_settings));
                                tracing::info!("config hot-reloaded");
                            }
                            Err(e) => tracing::warn!("config reload failed: {e}"),
                        }
                    }
                });
            }

            let app = crate::api::app::build_router(state);

            let bind_addr = format!("{}:{}", host, port);
            tracing::info!("Listening on {}", bind_addr);
            let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
            use std::net::SocketAddr;
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
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
                        api_key_id: None,
                        tag: None,
                        window: window.clone(),
                        limit_usd,
                        limit_tokens: None,
                        rate_rpm: None,
                        max_concurrent: None,
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
        Commands::Report(report_args) => {
            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;

            use crate::cli::commands::ReportCommands;

            match report_args.command {
                ReportCommands::Cost { user, tag, window, format } => {
                    let rows = if let Some(ref t) = tag {
                        crate::report::cost_by_tag_window(&db.pool, &window, t).await?
                    } else {
                        crate::report::cost_by_user_window(&db.pool, &window, user.as_deref())
                            .await?
                    };
                    print_rows(
                        &rows,
                        &["User", "Model", "Cost (USD)", "Tokens In", "Tokens Out", "Requests"],
                        |r| {
                            vec![
                                r.user_name.clone(),
                                r.model.clone(),
                                format!("{:.6}", r.total_cost_usd),
                                r.total_tokens_in.to_string(),
                                r.total_tokens_out.to_string(),
                                r.request_count.to_string(),
                            ]
                        },
                        format,
                    );
                }
                ReportCommands::Usage { model, project, since, format } => {
                    let rows =
                        crate::report::usage_by_model(&db.pool, since.as_deref(), model.as_deref(), project.as_deref()).await?;
                    print_rows(
                        &rows,
                        &["Model", "Provider", "Requests", "Tokens In", "Tokens Out", "Cost (USD)"],
                        |r| {
                            vec![
                                r.model.clone(),
                                r.provider.clone(),
                                r.request_count.to_string(),
                                r.total_tokens_in.to_string(),
                                r.total_tokens_out.to_string(),
                                format!("{:.6}", r.total_cost_usd),
                            ]
                        },
                        format,
                    );
                }
                ReportCommands::Prompts { user, limit, since, format } => {
                    let rows = crate::report::recent_prompts(
                        &db.pool,
                        user.as_deref(),
                        limit,
                        since.as_deref(),
                    )
                    .await?;
                    print_rows(
                        &rows,
                        &["ID", "User", "Request Model", "Routed Model", "Cost", "Created At"],
                        |r| {
                            vec![
                                r.id.to_string(),
                                r.user_name.clone(),
                                r.request_model.clone(),
                                r.routed_model.clone(),
                                format!("{:.6}", r.cost_usd),
                                r.created_at.clone(),
                            ]
                        },
                        format,
                    );
                }
                ReportCommands::Audit { actor, tail, format } => {
                    let rows =
                        crate::report::recent_audit(&db.pool, actor.as_deref(), tail).await?;
                    print_audit_rows(rows, format);
                }
                ReportCommands::Hooks { format } => {
                    let rows = crate::report::hook_latency_stats(&db.pool).await?;
                    print_rows(
                        &rows,
                        &["Hook", "Invocations", "Success %", "Avg ms", "p50 ms", "p95 ms", "p99 ms"],
                        |r| {
                            vec![
                                r.hook_name.clone(),
                                r.invocation_count.to_string(),
                                format!("{:.1}%", r.success_rate * 100.0),
                                format!("{:.1}", r.avg_duration_ms),
                                r.p50_duration_ms.to_string(),
                                r.p95_duration_ms.to_string(),
                                r.p99_duration_ms.to_string(),
                            ]
                        },
                        format,
                    );
                }
            }
        }
        Commands::Audit { tail, format } => {
            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;
            let rows = crate::report::recent_audit(&db.pool, None, tail).await?;
            print_audit_rows(rows, format);
        }
        Commands::InstallService => {
            install_service()?;
        }
        Commands::UninstallService => {
            uninstall_service()?;
        }
    }
    Ok(())
}
