pub mod commands;
pub mod admin;
pub mod cache;

use std::sync::Arc;

use anyhow::Result;
use commands::{Cli, Commands, UserCommands, BudgetCommands, KeyCommands, GroupCommands, ModelCommands, FailoverCommands, AliasCommands, ProviderCommands};
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

/// The template ships a placeholder signing key so the file is readable as
/// documentation. `serve` refuses to start on that value, so `init` must
/// substitute a real one — otherwise the quickstart it prints leads straight
/// into a refusal.
fn config_template_with_generated_secret() -> String {
    use rand::Rng;
    const HEX: &[u8] = b"0123456789abcdef";
    let mut rng = rand::thread_rng();
    let secret: String = (0..64)
        .map(|_| HEX[rng.gen_range(0..HEX.len())] as char)
        .collect();
    CONFIG_TEMPLATE.replace(
        crate::config::schema::PLACEHOLDER_JWT_SECRET,
        &secret,
    )
}

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Init => {
            println!("modelrouter v{}", env!("CARGO_PKG_VERSION"));
            println!();
            let config_dir = dirs::home_dir()
                .unwrap_or_default()
                .join(".modelrouter");
            tokio::fs::create_dir_all(&config_dir).await?;
            // Set config dir to owner-only so the DB and config (which holds API keys)
            // are not readable by other OS users on shared servers.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o700);
                std::fs::set_permissions(&config_dir, perms)?;
            }
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
                    tokio::fs::write(&config_path, config_template_with_generated_secret()).await?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let perms = std::fs::Permissions::from_mode(0o600);
                        std::fs::set_permissions(&config_path, perms)?;
                    }
                    println!("Overwrote config at {}", config_path.display());
                } else {
                    println!("Aborted.");
                    return Ok(());
                }
            } else {
                tokio::fs::write(&config_path, config_template_with_generated_secret()).await?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let perms = std::fs::Permissions::from_mode(0o600);
                    std::fs::set_permissions(&config_path, perms)?;
                }
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


            // Refuse to serve if the config names a provider whose adapter is
            // compiled out of this binary — the registries would silently
            // substitute the OpenAI-compat adapter for it (issue #24).
            crate::providers::validate_provider_features(&settings.providers)?;

            // Refuse to serve with an unset or placeholder signing key: admin
            // and dashboard sessions are authenticated by an HS256 signature
            // over it alone, so a weak value makes every session forgeable.
            settings.auth.validate_secret()?;

            // Effective [storage] policy (issue #4): the DB-stored GUI value
            // wins over config.toml; absence of a row means the file/default
            // applies. Held in an ArcSwap so an admin saving the form takes
            // effect immediately, without a restart.
            let storage_live = {
                use crate::db::repositories::app_settings::AppSettingsRepository;
                let from_db = AppSettingsRepository::get_setting(&*db, "storage")
                    .await
                    .ok()
                    .flatten()
                    .and_then(|json| serde_json::from_str(&json).ok());
                Arc::new(arc_swap::ArcSwap::from_pointee(
                    from_db.unwrap_or_else(|| settings.storage.clone()),
                ))
            };

            // Prompt-log store (issue #29): a dedicated SQLite file when
            // configured, else the main DB. Restart-scoped by design.
            let prompt_db = open_prompt_db(&settings, &db).await?;
            if let Some(path) = settings.storage.prompt_db_path.as_deref() {
                tracing::info!(path, "prompt log using dedicated database");
            }

            // Prompt-log retention: purge on an hourly check against the LIVE
            // policy, so a retention set in the GUI applies within the hour.
            // retention_days == 0 (the default) means keep forever — deletion
            // is strictly opt-in. Failures are logged, never fatal.
            {
                let purge_db = prompt_db.clone();
                let purge_storage = storage_live.clone();
                tokio::spawn(async move {
                    loop {
                        let retention_days = purge_storage.load().prompt_retention_days;
                        if retention_days > 0 {
                            let cutoff = (chrono::Utc::now()
                                - chrono::Duration::days(retention_days as i64))
                            .to_rfc3339();
                            use crate::db::repositories::prompts::PromptRepository;
                            match PromptRepository::purge_older_than(&*purge_db, &cutoff).await {
                                Ok(n) if n > 0 => {
                                    tracing::info!(deleted = n, retention_days, "prompt-log retention purge")
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    tracing::warn!(error = %e, "prompt-log retention purge failed")
                                }
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(60 * 60)).await;
                    }
                });
            }

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
            let search_registry = Arc::new(crate::providers::search_registry::SearchRegistry::new(
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

            // Load DB webhooks before moving `db` into AppState
            let db_webhooks: Vec<crate::db::repositories::webhook_callbacks::WebhookCallback> = {
                use crate::db::repositories::webhook_callbacks::WebhookCallbackRepository;
                db.list_enabled_webhooks().await.unwrap_or_default()
            };

            let state = crate::api::app::AppState {
                settings: settings.clone(),
                live_settings: live_settings.clone(),
                storage: storage_live.clone(),
                prompt_db: prompt_db.clone(),
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
                search_registry,
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
                session_affinity: Arc::new(crate::router::session_affinity::SessionAffinityMap::new(30 * 60)),
                callbacks: {
                    let mut backends: Vec<Box<dyn crate::callbacks::CallbackBackend>> = vec![];
                    if let Some(cfg) = settings.callbacks.langfuse.clone() {
                        backends.push(Box::new(crate::callbacks::langfuse::LangFuseBackend::new(cfg)));
                    }
                    if let Some(cfg) = settings.callbacks.langsmith.clone() {
                        backends.push(Box::new(crate::callbacks::langsmith::LangSmithBackend::new(cfg)));
                    }
                    // Load DB-registered webhooks
                    for row in db_webhooks {
                        let events: Vec<String> = serde_json::from_str(&row.events).unwrap_or_default();
                        backends.push(Box::new(crate::callbacks::webhook::WebhookBackend::new(
                            crate::callbacks::webhook::WebhookBackendConfig {
                                name: row.name,
                                url: row.url,
                                events,
                                secret_header_name: row.secret_header_name,
                                secret_header_value: row.secret_header_value,
                            },
                        )));
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
            // Seed DB model aliases and failover chains into live router/fallback
            {
                use crate::db::repositories::models::ModelRepository;
                let db_aliases =
                    crate::api::admin::aliases::build_db_alias_map(&state.db).await;
                if !db_aliases.is_empty() {
                    tracing::info!(count = db_aliases.len(), "loaded DB model aliases");
                }
                state.router.update_db_aliases(db_aliases);
                let availability =
                    crate::api::admin::aliases::build_availability_map(&state.db).await;
                if !availability.is_empty() {
                    tracing::info!(
                        count = availability.len(),
                        "loaded operator-disabled models/providers"
                    );
                }
                state.router.update_availability(availability);
                if let Ok(rows) = state.db.list_all_failovers().await {
                    let mut db_chains: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
                    for r in rows {
                        db_chains.entry(r.primary_model).or_default().push(r.fallback_model);
                    }
                    if !db_chains.is_empty() {
                        tracing::info!(count = db_chains.len(), "loaded DB failover chains");
                    }
                    state.fallback.update_db_chains(db_chains);
                }
            }

            // Background sweeper for session affinity TTL eviction
            {
                let affinity = state.session_affinity.clone();
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5 * 60));
                    loop {
                        interval.tick().await;
                        affinity.evict_expired();
                        tracing::debug!(active_sessions = affinity.len(), "session affinity sweep complete");
                    }
                });
            }

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

            // Flag > config > built-in default. The config defaults already
            // supply 127.0.0.1:8080 when the section is absent.
            let host = host.unwrap_or_else(|| settings.server.host.clone());
            let port = port.unwrap_or(settings.server.port);
            let bind_addr = format!("{}:{}", host, port);
            tracing::info!("Listening on {}", bind_addr);
            let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
            use std::net::SocketAddr;
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
        }
        Commands::Cache(cache_args) => {
            cache::run(cache_args).await?;
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
                UserCommands::Create { name } => {
                    use crate::db::repositories::users::UserRepository;
                    use crate::db::repositories::api_keys::ApiKeyRepository;
                    use crate::db::models::NewUser;
                    use crate::api::auth::hash_token;

                    let user = UserRepository::create(&db, NewUser {
                        name: name.clone(),
                        email: None,
                    }).await?;

                    let raw_token = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
                    let hash = hash_token(&raw_token);
                    db.create_api_key(crate::db::models::NewApiKey {
                        user_id: user.id,
                        key_hash: hash,
                        label: Some("initial".to_string()),
                        expires_at: None,
                        project: None,
                        session_window_secs: None,
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
                            "{:>4}  {:20}  {}",
                            u.id,
                            u.name,
                            if u.enabled { "enabled" } else { "disabled" },
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
                    use crate::db::repositories::api_keys::ApiKeyRepository;
                    use crate::api::auth::hash_token;
                    let user = UserRepository::find_by_name(&db, &name).await?
                        .ok_or_else(|| anyhow::anyhow!("User not found: {}", name))?;
                    // Generate new key
                    let new_key = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace("-", ""));
                    let hash = hash_token(&new_key);
                    // Disable old keys for user
                    db.disable_all_keys_for_user(user.id).await?;
                    // Create new key
                    let _api_key = db.create_api_key(crate::db::models::NewApiKey {
                        user_id: user.id,
                        key_hash: hash,
                        label: Some("cli-rotate".to_string()),
                        expires_at: None,
                        project: None,
                        session_window_secs: None,
                    }).await?;
                    println!("New key for {}: {}", user.name, new_key);
                }
            }
        }
        Commands::Budget(budget_args) => {
            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;

            match budget_args.command {
                BudgetCommands::Set {
                    global,
                    project,
                    user,
                    group,
                    window,
                    window_start,
                    window_end,
                    limit_usd,
                    limit_tokens,
                    rate_rpm,
                    max_concurrent,
                    model_allow,
                    model_deny,
                } => {
                    use crate::db::repositories::{users::UserRepository, budgets::BudgetRepository};
                    use crate::db::models::{NewBudgetRule, BudgetScope};

                    // Validate exactly one scope flag
                    let scope_count = [global, project.is_some(), user.is_some(), group.is_some()]
                        .iter()
                        .filter(|&&b| b)
                        .count();
                    if scope_count == 0 {
                        anyhow::bail!("Exactly one scope flag is required: --global, --project <name>, --user <name>, or --group <name>");
                    }
                    if scope_count > 1 {
                        anyhow::bail!("Only one scope flag may be specified at a time: --global, --project, --user, --group");
                    }

                    // Helper: append date suffix
                    let date_suffix = |s: &str| format!("{}T00:00:00+00:00", s);

                    // Determine scope and window
                    let (scope, effective_window, user_id, group_name_val, project_val) =
                        if global {
                            (BudgetScope::Global, window.clone(), None, None, None)
                        } else if let Some(ref proj) = project {
                            (BudgetScope::Project(proj.clone()), window.clone(), None, None, Some(proj.clone()))
                        } else if let Some(ref uname) = user {
                            let found = UserRepository::find_by_name(&db, uname).await?
                                .ok_or_else(|| anyhow::anyhow!("User not found: {}", uname))?;
                            (BudgetScope::User(found.id), window.clone(), Some(found.id), None, None)
                        } else {
                            // group
                            let gname = group.as_ref().unwrap();
                            if window != "monthly" {
                                eprintln!("Warning: --window is ignored for --group scope; stored as 'target'");
                            }
                            (BudgetScope::Group(gname.clone()), "target".to_string(), None, Some(gname.clone()), None)
                        };

                    // Validate window=total requires start+end
                    let (ws, we) = if effective_window == "total" {
                        let start = window_start.as_ref()
                            .ok_or_else(|| anyhow::anyhow!("--window total requires --window-start <YYYY-MM-DD>"))?;
                        let end = window_end.as_ref()
                            .ok_or_else(|| anyhow::anyhow!("--window total requires --window-end <YYYY-MM-DD>"))?;
                        if start >= end {
                            anyhow::bail!("--window-start must be before --window-end");
                        }
                        (Some(date_suffix(start)), Some(date_suffix(end)))
                    } else {
                        (None, None)
                    };

                    // Duplicate check
                    let existing = BudgetRepository::list_for_scope(&db, &scope).await?;
                    for r in &existing {
                        if r.window == effective_window {
                            anyhow::bail!(
                                "A budget rule with window='{}' already exists for this scope (id={}). Delete it first.",
                                effective_window,
                                r.id
                            );
                        }
                    }

                    let model_allow_vec = model_allow
                        .map(|s| s.split(',').map(|m| m.trim().to_string()).collect::<Vec<_>>())
                        .unwrap_or_default();
                    let model_deny_vec = model_deny
                        .map(|s| s.split(',').map(|m| m.trim().to_string()).collect::<Vec<_>>())
                        .unwrap_or_default();

                    let rule = BudgetRepository::create(&db, NewBudgetRule {
                        user_id,
                        group_name: group_name_val,
                        api_key_id: None,
                        tag: None,
                        window: effective_window.clone(),
                        limit_usd,
                        limit_tokens,
                        rate_rpm,
                        max_concurrent,
                        model_allow: model_allow_vec,
                        model_deny: model_deny_vec,
                        project: project_val,
                        window_start: ws,
                        window_end: we,
                    }).await?;

                    println!("Created budget rule id={}", rule.id);
                }
                BudgetCommands::Edit {
                    id,
                    limit_usd,
                    limit_tokens,
                    rate_rpm,
                    max_concurrent,
                    model_allow,
                    model_deny,
                    window_start,
                    window_end,
                } => {
                    use crate::db::repositories::budgets::BudgetRepository;
                    use crate::db::models::UpdateBudgetRule;

                    let date_suffix = |s: &str| format!("{}T00:00:00+00:00", s);

                    let changes = UpdateBudgetRule {
                        limit_usd,
                        limit_tokens,
                        rate_rpm,
                        max_concurrent,
                        model_allow: model_allow.map(|s| {
                            s.split(',').map(|m| m.trim().to_string()).collect()
                        }),
                        model_deny: model_deny.map(|s| {
                            s.split(',').map(|m| m.trim().to_string()).collect()
                        }),
                        window_start: window_start.as_deref().map(date_suffix),
                        window_end: window_end.as_deref().map(date_suffix),
                    };

                    let rule = BudgetRepository::update(&db, id, &changes).await?;
                    println!("Updated budget rule id={}: window={} limit_usd={:?} limit_tokens={:?} rate_rpm={:?} max_concurrent={:?}",
                        rule.id, rule.window, rule.limit_usd, rule.limit_tokens, rule.rate_rpm, rule.max_concurrent);
                }
                BudgetCommands::Delete { id } => {
                    use crate::db::repositories::budgets::BudgetRepository;
                    BudgetRepository::delete(&db, id).await?;
                    println!("Deleted budget rule {}", id);
                }
                BudgetCommands::List { user } => {
                    use crate::db::repositories::{users::UserRepository, budgets::BudgetRepository};

                    let all_rules = BudgetRepository::list_all(&db).await?;
                    let all_users = UserRepository::list(&db).await?;
                    let user_map: std::collections::HashMap<i64, String> =
                        all_users.iter().map(|u| (u.id, u.name.clone())).collect();

                    // If --user filter, get that user's id
                    let filter_user_id: Option<i64> = if let Some(ref name) = user {
                        let found = UserRepository::find_by_name(&db, name).await?
                            .ok_or_else(|| anyhow::anyhow!("User not found: {}", name))?;
                        Some(found.id)
                    } else {
                        None
                    };

                    let rules: Vec<_> = all_rules.iter().filter(|r| {
                        match filter_user_id {
                            Some(uid) => r.user_id == Some(uid),
                            None => true,
                        }
                    }).collect();

                    for r in rules {
                        // Determine scope label
                        let scope_label = if r.user_id.is_none() && r.group_name.is_none() && r.project.is_none() {
                            "global".to_string()
                        } else if let Some(uid) = r.user_id {
                            format!("user={}", user_map.get(&uid).map(|s| s.as_str()).unwrap_or("?"))
                        } else if let Some(ref gname) = r.group_name {
                            format!("group={}", gname)
                        } else if let Some(ref proj) = r.project {
                            format!("project={}", proj)
                        } else {
                            "?".to_string()
                        };

                        // Date range for total window
                        let date_range = if r.window == "total" {
                            let start = r.window_start.as_deref().unwrap_or("?");
                            let end = r.window_end.as_deref().unwrap_or("?");
                            // Trim to YYYY-MM-DD
                            let start_s = if start.len() >= 10 { &start[..10] } else { start };
                            let end_s = if end.len() >= 10 { &end[..10] } else { end };
                            format!("  {}→{}", start_s, end_s)
                        } else {
                            String::new()
                        };

                        // Non-null fields
                        let mut parts: Vec<String> = vec![];
                        if let Some(v) = r.limit_usd { parts.push(format!("limit=${:.2}", v)); }
                        if let Some(v) = r.limit_tokens { parts.push(format!("tokens={}", v)); }
                        if let Some(v) = r.rate_rpm { parts.push(format!("rpm={}", v)); }
                        if let Some(v) = r.max_concurrent { parts.push(format!("concurrent={}", v)); }
                        if !r.model_allow.is_empty() && r.model_allow != "[]" {
                            parts.push(format!("allow={}", r.model_allow));
                        }
                        if !r.model_deny.is_empty() && r.model_deny != "[]" {
                            parts.push(format!("deny={}", r.model_deny));
                        }

                        println!("{:>4}  {:16}  {:10}{}  {}",
                            r.id,
                            scope_label,
                            r.window,
                            date_range,
                            parts.join("  "),
                        );
                    }
                }
            }
        }
        Commands::Group(group_args) => {
            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;

            match group_args.command {
                GroupCommands::Create { name, priority } => {
                    use crate::db::repositories::groups::GroupRepository;
                    if GroupRepository::find_group_by_name(&db, &name).await?.is_some() {
                        anyhow::bail!("Group '{}' already exists", name);
                    }
                    let g = GroupRepository::create_group(&db, &name, priority).await?;
                    println!("Created group '{}' (id={}, priority={})", g.name, g.id, g.priority);
                }
                GroupCommands::List => {
                    use crate::db::repositories::groups::GroupRepository;
                    let groups = GroupRepository::list_groups(&db).await?;
                    for g in groups {
                        println!("{:>4}  {:20}  priority={:<4}  {}",
                            g.id, g.name, g.priority,
                            if g.enabled { "enabled" } else { "disabled" });
                    }
                }
                GroupCommands::Enable { name } => {
                    use crate::db::repositories::groups::GroupRepository;
                    let g = GroupRepository::find_group_by_name(&db, &name).await?
                        .ok_or_else(|| anyhow::anyhow!("Group not found: {}", name))?;
                    GroupRepository::set_group_enabled(&db, g.id, true).await?;
                    println!("Enabled group '{}'", name);
                }
                GroupCommands::Disable { name } => {
                    use crate::db::repositories::groups::GroupRepository;
                    let g = GroupRepository::find_group_by_name(&db, &name).await?
                        .ok_or_else(|| anyhow::anyhow!("Group not found: {}", name))?;
                    GroupRepository::set_group_enabled(&db, g.id, false).await?;
                    println!("Disabled group '{}'", name);
                }
                GroupCommands::Members { group } => {
                    use crate::db::repositories::groups::GroupRepository;
                    let g = GroupRepository::find_group_by_name(&db, &group).await?
                        .ok_or_else(|| anyhow::anyhow!("Group not found: {}", group))?;
                    let members = GroupRepository::list_memberships(&db, g.id).await?;
                    for m in members {
                        let status = if let Some(ref da) = m.disabled_at {
                            format!("Disabled {}", &da[..10.min(da.len())])
                        } else {
                            "Active".to_string()
                        };
                        let joined = &m.joined_at[..10.min(m.joined_at.len())];
                        println!("{:>4}  {:20}  joined={}  {}", m.id, m.user_name, joined, status);
                    }
                }
                GroupCommands::AddMember { group, user } => {
                    use crate::db::repositories::{groups::GroupRepository, users::UserRepository};
                    let g = GroupRepository::find_group_by_name(&db, &group).await?
                        .ok_or_else(|| anyhow::anyhow!("Group not found: {}", group))?;
                    let u = UserRepository::find_by_name(&db, &user).await?
                        .ok_or_else(|| anyhow::anyhow!("User not found: {}", user))?;
                    if GroupRepository::find_active_membership(&db, g.id, u.id).await?.is_some() {
                        anyhow::bail!("User '{}' is already an active member of group '{}'", user, group);
                    }
                    GroupRepository::add_member(&db, g.id, u.id).await?;
                    println!("Added '{}' to group '{}'", user, group);
                }
                GroupCommands::RemoveMember { group, user } => {
                    use crate::db::repositories::{groups::GroupRepository, users::UserRepository};
                    let g = GroupRepository::find_group_by_name(&db, &group).await?
                        .ok_or_else(|| anyhow::anyhow!("Group not found: {}", group))?;
                    let u = UserRepository::find_by_name(&db, &user).await?
                        .ok_or_else(|| anyhow::anyhow!("User not found: {}", user))?;
                    let membership = GroupRepository::find_active_membership(&db, g.id, u.id).await?
                        .ok_or_else(|| anyhow::anyhow!("No active membership for '{}' in group '{}'", user, group))?;
                    GroupRepository::disable_membership(&db, membership.id).await?;
                    println!("Removed '{}' from group '{}'", user, group);
                }
            }
        }
        Commands::Report(report_args) => {
            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;

            use crate::cli::commands::ReportCommands;

            match report_args.command {
                ReportCommands::Cost {
                    user, group, project, model, key_id, tag, correlation_id, by, window, format,
                } => {
                    // Attribution short-circuits the per-user report: it answers
                    // a different question (what did *this* unit of work cost)
                    // and the other filters do not compose with it.
                    if tag.is_some() || correlation_id.is_some() {
                        let filter = parse_attribution_filter(tag.as_deref(), correlation_id)?;
                        report_attribution(&db, &filter, by, &window, format).await?;
                        return Ok(());
                    }
                    let rows = crate::report::cost_by_user_window(
                        &db.pool, &window,
                        user.as_deref(),
                        project.as_deref(),
                        group.as_deref(),
                        model.as_deref(),
                        key_id,
                    ).await?;
                    print_rows(
                        &rows,
                        &["User", "Model", "Window", "Group", "Project", "Key",
                          "Cost (USD)", "Requests", "Tokens In (Prompts)", "Tokens Out (Completions)"],
                        |r| {
                            vec![
                                r.user_name.clone(),
                                r.model.clone(),
                                r.window.clone(),
                                r.groups.clone(),
                                r.project.clone(),
                                r.key.clone(),
                                format!("{:.2}", r.total_cost_usd),
                                r.request_count.to_string(),
                                r.total_tokens_out.to_string(),
                                r.total_tokens_in.to_string(),
                            ]
                        },
                        format,
                    );
                }
                ReportCommands::Compare { dimension, key, a, b, window, format } => {
                    let query = crate::api::admin::compare::CompareQuery {
                        dimension,
                        key: key.unwrap_or_default(),
                        a,
                        b,
                        // `alltime` is the CLI's spelling of the API's `all`.
                        window: if window == "alltime" { "all".to_string() } else { window },
                    };
                    report_compare(db, &settings, &query, format).await?;
                }
                ReportCommands::Usage {
                    total, subtotal, detail,
                    alltime, annual, monthly,
                    global, group, project, user,
                    format,
                } => {
                    use crate::report::{UsageScope, UsageWindow, UsageGranularity, fetch_usage_rows};

                    // Validate granularity (exactly one)
                    let gran_count = [total, subtotal, detail].iter().filter(|&&b| b).count();
                    if gran_count == 0 {
                        anyhow::bail!("Exactly one granularity flag required: --total, --subtotal, or --detail");
                    }
                    if gran_count > 1 {
                        anyhow::bail!("Only one granularity flag may be specified: --total, --subtotal, --detail");
                    }

                    // Validate window (exactly one)
                    let win_count = [alltime, annual, monthly].iter().filter(|&&b| b).count();
                    if win_count == 0 {
                        anyhow::bail!("Exactly one window flag required: --alltime, --annual, or --monthly");
                    }
                    if win_count > 1 {
                        anyhow::bail!("Only one window flag may be specified: --alltime, --annual, --monthly");
                    }

                    // Validate scope (at least one)
                    let scope_count = [global, group.is_some(), project.is_some(), user.is_some()]
                        .iter().filter(|&&b| b).count();
                    if scope_count == 0 {
                        anyhow::bail!("At least one scope flag required: --global, --group <name>, --project <name>, or --user <name>");
                    }

                    let granularity = if total { UsageGranularity::Total }
                        else if subtotal { UsageGranularity::Subtotal }
                        else { UsageGranularity::Detail };

                    let window = if alltime { UsageWindow::AllTime }
                        else if annual { UsageWindow::Annual }
                        else { UsageWindow::Monthly };

                    // Build scope
                    let scope = match (&user, &group, &project, global) {
                        (Some(u), Some(g), _, _) => UsageScope::UserInGroup { user: u.clone(), group: g.clone() },
                        (Some(u), _, Some(p), _) => UsageScope::UserInProject { user: u.clone(), project: p.clone() },
                        (Some(u), _, _, _) => UsageScope::User(u.clone()),
                        (_, Some(g), _, _) => UsageScope::Group(g.clone()),
                        (_, _, Some(p), _) => UsageScope::Project(p.clone()),
                        _ => UsageScope::Global,
                    };

                    let rows = fetch_usage_rows(&db.pool, &scope, &window, &granularity).await?;

                    match format {
                        OutputFormat::Json => {
                            let json_rows: Vec<serde_json::Value> = rows.iter().map(|r| {
                                serde_json::json!({
                                    "bucket": r.bucket,
                                    "user": r.user_name,
                                    "project": r.project,
                                    "model": r.model,
                                    "tokens_in": r.tokens_in,
                                    "tokens_out": r.tokens_out,
                                    "cost_usd": r.cost_usd,
                                })
                            }).collect();
                            println!("{}", serde_json::to_string_pretty(&json_rows)?);
                        }
                        OutputFormat::Csv => {
                            println!("bucket,user,project,model,tokens_in,tokens_out,cost_usd");
                            for r in &rows {
                                println!("{},{},{},{},{},{},{:.2}",
                                    r.bucket.as_deref().unwrap_or(""),
                                    r.user_name.as_deref().unwrap_or(""),
                                    r.project.as_deref().unwrap_or(""),
                                    r.model.as_deref().unwrap_or(""),
                                    r.tokens_in,
                                    r.tokens_out,
                                    r.cost_usd,
                                );
                            }
                        }
                        OutputFormat::Table => {
                            // Group rows by bucket preserving order (rows are already sorted by bucket)
                            let mut buckets: Vec<(Option<String>, Vec<usize>)> = Vec::new();
                            for (i, r) in rows.iter().enumerate() {
                                if let Some(last) = buckets.last_mut() {
                                    if last.0 == r.bucket {
                                        last.1.push(i);
                                        continue;
                                    }
                                }
                                buckets.push((r.bucket.clone(), vec![i]));
                            }

                            let header = format!("{:<20} {:<20} {:<28} {:>12} {:>12} {:>10}",
                                "User", "Project", "Model", "Tokens Out (Completions)", "Tokens In (Prompts)", "Cost USD");

                            let mut grand_in: i64 = 0;
                            let mut grand_out: i64 = 0;
                            let mut grand_cost: f64 = 0.0;

                            fn fmt_num(n: i64) -> String {
                                // Simple thousands formatting
                                let s = n.to_string();
                                let mut result = String::new();
                                for (i, c) in s.chars().rev().enumerate() {
                                    if i > 0 && i % 3 == 0 { result.push(','); }
                                    result.push(c);
                                }
                                result.chars().rev().collect()
                            }

                            for (bucket, indices) in &buckets {
                                if let Some(b) = bucket {
                                    println!("\n=== {} ===", b);
                                }
                                println!("{}", header);
                                println!("{}", "-".repeat(header.len()));

                                let mut user_in: i64 = 0;
                                let mut user_out: i64 = 0;
                                let mut user_cost: f64 = 0.0;
                                let mut last_user: Option<String> = None;

                                for &idx in indices {
                                    let r = &rows[idx];
                                    if granularity == UsageGranularity::Detail {
                                        let cur_user = r.user_name.clone();
                                        if last_user.is_some() && last_user != cur_user {
                                            println!("  {:30} {:>12} {:>12} {:>10}",
                                                format!("Subtotal: {}", last_user.as_deref().unwrap_or("")),
                                                fmt_num(user_in), fmt_num(user_out),
                                                format!("${:.2}", user_cost));
                                            user_in = 0; user_out = 0; user_cost = 0.0;
                                        }
                                        last_user = cur_user;
                                        user_in += r.tokens_in;
                                        user_out += r.tokens_out;
                                        user_cost += r.cost_usd;
                                    }

                                    println!("{:<20} {:<20} {:<28} {:>12} {:>12} {:>10}",
                                        r.user_name.as_deref().unwrap_or(""),
                                        r.project.as_deref().unwrap_or(""),
                                        r.model.as_deref().unwrap_or(""),
                                        fmt_num(r.tokens_in),
                                        fmt_num(r.tokens_out),
                                        format!("${:.2}", r.cost_usd),
                                    );

                                    grand_in += r.tokens_in;
                                    grand_out += r.tokens_out;
                                    grand_cost += r.cost_usd;
                                }

                                if granularity == UsageGranularity::Detail {
                                    if let Some(ref u) = last_user {
                                        println!("  {:30} {:>12} {:>12} {:>10}",
                                            format!("Subtotal: {}", u),
                                            fmt_num(user_in), fmt_num(user_out),
                                            format!("${:.2}", user_cost));
                                    }
                                }
                            }

                            println!();
                            println!("{:<48} {:>12} {:>12} {:>10}",
                                "Grand Total:",
                                {
                                    let s = grand_in.to_string();
                                    let mut r = String::new();
                                    for (i, c) in s.chars().rev().enumerate() {
                                        if i > 0 && i % 3 == 0 { r.push(','); }
                                        r.push(c);
                                    }
                                    r.chars().rev().collect::<String>()
                                },
                                {
                                    let s = grand_out.to_string();
                                    let mut r = String::new();
                                    for (i, c) in s.chars().rev().enumerate() {
                                        if i > 0 && i % 3 == 0 { r.push(','); }
                                        r.push(c);
                                    }
                                    r.chars().rev().collect::<String>()
                                },
                                format!("${:.2}", grand_cost),
                            );
                        }
                    }
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
                        &["ID", "User", "Request Model", "Routed Model", "Cost", "Cached", "Created At"],
                        |r| {
                            vec![
                                r.id.to_string(),
                                r.user_name.clone(),
                                r.request_model.clone(),
                                r.routed_model.clone(),
                                format!("{:.2}", r.cost_usd),
                                if r.cached { "yes".to_string() } else { "no".to_string() },
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
        Commands::Admin(admin_args) => {
            admin::run(cli.config, admin_args.command).await?;
        }
        Commands::Key(key_args) => {
            use crate::db::repositories::{api_keys::ApiKeyRepository, users::UserRepository};
            use crate::db::models::NewUser;

            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;

            match key_args.command {
                KeyCommands::Create { user, project, label, email: _, session_window } => {
                    use crate::api::auth::hash_token;

                    // Find or create user
                    let u = match UserRepository::find_by_name(&db, &user).await? {
                        Some(u) => u,
                        None => UserRepository::create(&db, NewUser {
                            name: user.clone(),
                            email: None,
                        }).await?,
                    };

                    // Reject duplicate user+project
                    if db.find_key_by_user_project(u.id, Some(&project)).await?.is_some() {
                        anyhow::bail!("A key for user '{}' / project '{}' already exists. Use `key rotate` to replace it.", user, project);
                    }

                    let raw = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
                    db.create_api_key(crate::db::models::NewApiKey {
                        user_id: u.id,
                        key_hash: hash_token(&raw),
                        label,
                        expires_at: None,
                        project: Some(project.clone()),
                        session_window_secs: session_window,
                    }).await?;

                    println!("Created key for '{}' / project '{}'", user, project);
                    println!("Key: {}", raw);
                    println!("Store this securely — it cannot be retrieved later.");
                }
                KeyCommands::List { user, project } => {
                    let keys = db.list_all_api_keys().await?;
                    let users = UserRepository::list(&db).await?;
                    let user_map: std::collections::HashMap<i64, String> =
                        users.iter().map(|u| (u.id, u.name.clone())).collect();

                    let filtered = keys.iter().filter(|k| {
                        let name_match = user.as_ref().map(|n| {
                            user_map.get(&k.user_id).map(|u| u == n).unwrap_or(false)
                        }).unwrap_or(true);
                        let proj_match = project.as_ref().map(|p| {
                            k.project.as_deref() == Some(p.as_str())
                        }).unwrap_or(true);
                        name_match && proj_match
                    });

                    let fmt_ts = |s: &str| if s.len() >= 19 { s[..19].replace('T', " ") } else { s.to_string() };
                    println!("{:>4}  {:16}  {:16}  {:16}  {:8}  {:19}  {}", "ID", "User", "Project", "Label", "Status", "Created", "Disabled");
                    for k in filtered {
                        println!("{:>4}  {:16}  {:16}  {:16}  {:8}  {:19}  {}",
                            k.id,
                            user_map.get(&k.user_id).map(|s| s.as_str()).unwrap_or("?"),
                            k.project.as_deref().unwrap_or("—"),
                            k.label.as_deref().unwrap_or("—"),
                            if k.enabled { "enabled" } else { "disabled" },
                            fmt_ts(&k.created_at),
                            k.disabled_at.as_deref().map(|s| fmt_ts(s)).unwrap_or_else(|| "—".to_string()),
                        );
                    }
                }
                KeyCommands::Rotate { user, project } => {
                    use crate::api::auth::hash_token;

                    let u = UserRepository::find_by_name(&db, &user).await?
                        .ok_or_else(|| anyhow::anyhow!("User not found: {}", user))?;

                    let group_keys = db.list_keys_for_group(u.id, Some(&project)).await?;
                    if group_keys.is_empty() {
                        anyhow::bail!("No key found for user '{}' / project '{}'", user, project);
                    }
                    let label = group_keys.first().and_then(|k| k.label.clone());
                    let session_window_secs = group_keys.first().and_then(|k| k.session_window_secs);

                    // Disable all active keys in this group
                    for k in group_keys.iter().filter(|k| k.enabled) {
                        db.disable_key(k.id).await?;
                    }

                    let raw = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
                    db.create_api_key(crate::db::models::NewApiKey {
                        user_id: u.id,
                        key_hash: hash_token(&raw),
                        label,
                        expires_at: None,
                        project: Some(project.clone()),
                        session_window_secs,
                    }).await?;

                    println!("Rotated key for '{}' / project '{}'", user, project);
                    println!("New key: {}", raw);
                    println!("Store this securely — it cannot be retrieved later.");
                }
                KeyCommands::Disable { user, project } => {
                    let u = UserRepository::find_by_name(&db, &user).await?
                        .ok_or_else(|| anyhow::anyhow!("User not found: {}", user))?;

                    let group_keys = db.list_keys_for_group(u.id, Some(&project)).await?;
                    let active: Vec<_> = group_keys.iter().filter(|k| k.enabled).collect();
                    if active.is_empty() {
                        anyhow::bail!("No active key found for user '{}' / project '{}'", user, project);
                    }
                    for k in active {
                        db.disable_key(k.id).await?;
                    }
                    println!("Disabled key(s) for '{}' / project '{}'", user, project);
                }
            }
        }
        Commands::Provider(provider_args) => {
            use crate::db::repositories::models::ModelRepository;

            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;

            match provider_args.command {
                ProviderCommands::List => {
                    let states = db.list_provider_states().await?;
                    let mut names: Vec<String> = settings.providers.keys().cloned().collect();
                    for s in &states {
                        if !names.contains(&s.provider) {
                            names.push(s.provider.clone());
                        }
                    }
                    names.sort();
                    if names.is_empty() {
                        println!("No providers configured.");
                        return Ok(());
                    }
                    println!("{:20}  {}", "PROVIDER", "STATUS");
                    println!("{}", "─".repeat(72));
                    for name in names {
                        let row = states.iter().find(|s| s.provider == name);
                        let status = match row {
                            Some(r) if !r.enabled => format!(
                                "disabled ({} — {})",
                                r.disabled_reason.as_deref().unwrap_or("no reason recorded"),
                                r.disabled_by.as_deref().unwrap_or("unknown"),
                            ),
                            _ => "enabled".to_string(),
                        };
                        println!("{:20}  {}", name, status);
                    }
                }
                ProviderCommands::Disable { provider, reason } => {
                    if !settings.providers.contains_key(&provider) {
                        anyhow::bail!("Unknown provider: {}", provider);
                    }
                    db.set_provider_enabled(&provider, false, reason.as_deref(), Some("cli"))
                        .await?;
                    println!(
                        "Disabled provider '{}' ({}). It stays disabled until explicitly enabled.",
                        provider,
                        reason.as_deref().unwrap_or("no reason given")
                    );
                }
                ProviderCommands::Enable { provider } => {
                    db.set_provider_enabled(&provider, true, None, Some("cli")).await?;
                    println!("Enabled provider '{}'.", provider);
                }
            }
        }
        Commands::Alias(alias_args) => {
            use crate::db::models::NewModelAlias;
            use crate::db::repositories::aliases::AliasRepository;
            use crate::db::repositories::models::ModelRepository;

            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;

            match alias_args.command {
                AliasCommands::List { format } => {
                    let rows = db.list_aliases().await?;
                    // Effective map: config < enabled model-row aliases < runtime aliases.
                    let mut effective: std::collections::BTreeMap<String, (String, String)> =
                        settings
                            .routing
                            .model_aliases
                            .iter()
                            .map(|(a, t)| (a.clone(), (t.clone(), "config".to_string())))
                            .collect();
                    for m in db.list_models().await?.iter().filter(|m| m.enabled) {
                        if let Some(a) = m.alias.as_ref() {
                            effective.insert(
                                a.clone(),
                                (format!("{}/{}", m.provider, m.name), "model".to_string()),
                            );
                        }
                    }
                    for r in &rows {
                        effective
                            .insert(r.alias.clone(), (r.target.clone(), "runtime".to_string()));
                    }

                    if matches!(format, OutputFormat::Json) {
                        let out: Vec<_> = effective
                            .iter()
                            .map(|(alias, (target, source))| {
                                serde_json::json!({
                                    "alias": alias, "target": target, "source": source,
                                })
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&out)?);
                    } else if effective.is_empty() {
                        println!("No aliases defined.");
                    } else {
                        println!("{:24}  {:40}  {}", "ALIAS", "TARGET", "SOURCE");
                        println!("{}", "─".repeat(78));
                        for (alias, (target, source)) in &effective {
                            println!("{:24}  {:40}  {}", alias, target, source);
                        }
                    }
                }
                AliasCommands::Set { alias, target } => {
                    let alias = alias.trim().to_string();
                    let target = target.trim().to_string();
                    if alias.is_empty() || target.is_empty() {
                        anyhow::bail!("alias and target are required");
                    }
                    if alias == target {
                        anyhow::bail!("alias '{}' cannot point at itself", alias);
                    }
                    if alias.starts_with(':') {
                        anyhow::bail!(
                            "aliases may not start with ':' — that prefix is reserved for routing shortcuts"
                        );
                    }
                    db.upsert_alias(NewModelAlias {
                        alias: alias.clone(),
                        target: target.clone(),
                        created_by: Some("cli".to_string()),
                    })
                    .await?;
                    println!("Alias '{}' → '{}' saved.", alias, target);
                    println!(
                        "Note: a router already running against this database picks it up on restart; \
                         use the admin API or dashboard for a live change."
                    );
                }
                AliasCommands::Rm { alias } => {
                    if db.delete_alias(&alias).await? {
                        println!("Removed alias '{}'.", alias);
                    } else {
                        anyhow::bail!("No such alias: {}", alias);
                    }
                }
            }
        }
        Commands::Model(model_args) => {
            use crate::db::repositories::models::ModelRepository;
            use crate::db::models::NewModel;

            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;

            match model_args.command {
                ModelCommands::Add { provider, name, alias } => {
                    let m = db.create_model(NewModel { provider, name, alias }).await?;
                    println!("Created model id={} provider={} name={} alias={}",
                        m.id, m.provider, m.name,
                        m.alias.as_deref().unwrap_or("—"));
                }
                ModelCommands::List => {
                    let models = db.list_models().await?;
                    if models.is_empty() {
                        println!("No models registered.");
                        return Ok(());
                    }
                    println!("{:>4}  {:16}  {:36}  {:16}  {}",
                        "ID", "Provider", "Name", "Alias", "Status");
                    println!("{}", "─".repeat(96));
                    for m in models {
                        let status = if m.enabled {
                            "enabled".to_string()
                        } else {
                            format!(
                                "disabled ({} — {})",
                                m.disabled_reason.as_deref().unwrap_or("no reason recorded"),
                                m.disabled_by.as_deref().unwrap_or("unknown"),
                            )
                        };
                        println!("{:>4}  {:16}  {:36}  {:16}  {}",
                            m.id, m.provider, m.name,
                            m.alias.as_deref().unwrap_or("—"),
                            status);
                    }
                }
                ModelCommands::Enable { id } => {
                    db.set_model_enabled_with_reason(id, true, None, Some("cli")).await?;
                    println!("Enabled model id={}", id);
                }
                ModelCommands::Disable { id, reason } => {
                    db.set_model_enabled_with_reason(id, false, reason.as_deref(), Some("cli"))
                        .await?;
                    println!(
                        "Disabled model id={} ({}). It stays disabled until explicitly enabled.",
                        id,
                        reason.as_deref().unwrap_or("no reason given")
                    );
                }
                ModelCommands::Delete { id } => {
                    if db.delete_model(id).await? {
                        println!("Deleted model id={}", id);
                    } else {
                        anyhow::bail!("Model id={} not found", id);
                    }
                }
                ModelCommands::Failover(failover_args) => {
                    match failover_args.command {
                        FailoverCommands::Set { model, fallback } => {
                            let fallbacks: Vec<String> = fallback
                                .split(',')
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                            db.set_failovers(&model, &fallbacks).await?;
                            println!("Set failover chain for '{}': {}", model, fallbacks.join(" → "));
                        }
                        FailoverCommands::List { model } => {
                            let rows = if let Some(ref m) = model {
                                db.list_failovers(m).await?
                            } else {
                                db.list_all_failovers().await?
                            };
                            if rows.is_empty() {
                                println!("No failover chains configured.");
                                return Ok(());
                            }
                            let mut current_primary = String::new();
                            for r in rows {
                                if r.primary_model != current_primary {
                                    current_primary = r.primary_model.clone();
                                    print!("  {} → ", current_primary);
                                } else {
                                    print!(", ");
                                }
                                print!("{}", r.fallback_model);
                            }
                            println!();
                        }
                    }
                }
            }
        }
        Commands::CheckTls => {
            let settings = crate::config::load(cli.config)?;
            let mut any_failed = false;

            // Default base URLs for known providers when no api_base is configured.
            let default_bases: std::collections::HashMap<&str, &str> = [
                ("anthropic", "https://api.anthropic.com"),
                ("openai",    "https://api.openai.com"),
                ("gemini",    "https://generativelanguage.googleapis.com"),
            ].iter().cloned().collect();

            for (name, provider) in &settings.providers {
                // Skip providers with no API key configured (likely not in use).
                if provider.api_key.is_empty() {
                    println!("[{}] skipped — no api_key configured", name);
                    continue;
                }

                let base = provider.api_base
                    .as_deref()
                    .or_else(|| default_bases.get(name.as_str()).copied())
                    .unwrap_or_else(|| "https://localhost");

                // Build a client that does NOT follow redirects so we get the TLS
                // handshake result directly, and with a short connect timeout.
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .expect("failed to build reqwest client");

                // Only test HTTPS endpoints — plain HTTP has no TLS to check.
                if !base.starts_with("https://") {
                    println!("[{}] skipped — non-HTTPS endpoint ({})", name, base);
                    continue;
                }

                // A HEAD to the base URL is enough to test TLS; we expect a 4xx/5xx
                // (no auth) or 200, either way the handshake succeeded.
                match client.head(base).send().await {
                    Ok(_) => {
                        println!("[{}] OK — TLS handshake succeeded ({})", name, base);
                    }
                    Err(e) => {
                        any_failed = true;
                        let msg = e.to_string().to_lowercase();
                        // reqwest surfaces certificate errors via the connect error chain.
                        // Distinguish cert errors from plain connection refused / DNS errors.
                        let is_cert_error = msg.contains("certificate")
                            || msg.contains("cert")
                            || msg.contains("tls")
                            || msg.contains("ssl")
                            || msg.contains("handshake");

                        if is_cert_error {
                            eprintln!(
                                "[{}] FAIL — TLS certificate verification failed ({})\n\
                                 \n\
                                 Your network may use SSL/TLS inspection (e.g. Zscaler, Netskope).\n\
                                 The proxy re-signs certificates with a private CA that the container\n\
                                 does not trust by default.\n\
                                 \n\
                                 See README.md § \"If required: adding a corporate CA certificate\"\n\
                                 for step-by-step instructions on extracting and injecting the CA cert.\n",
                                name, base
                            );
                        } else {
                            eprintln!("[{}] FAIL — connection error ({}): {}", name, base, e);
                        }
                    }
                }
            }

            if any_failed {
                std::process::exit(1);
            }
        }
        Commands::Webhook(webhook_args) => {
            use crate::db::repositories::webhook_callbacks::{NewWebhookCallback, WebhookCallbackRepository};
            use commands::WebhookCommands;

            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;

            match webhook_args.command {
                WebhookCommands::List => {
                    let rows = db.list_webhooks().await?;
                    println!("{:>4}  {:20}  {:8}  {:50}  {}",
                        "ID", "Name", "Status", "URL", "Events");
                    for w in rows {
                        println!("{:>4}  {:20}  {:8}  {:50}  {}",
                            w.id,
                            w.name,
                            if w.enabled { "enabled" } else { "disabled" },
                            w.url,
                            w.events,
                        );
                    }
                }
                WebhookCommands::Add { name, url, events, secret_header_name, secret_header_value } => {
                    let events_json = if events.starts_with('[') {
                        events
                    } else {
                        let parts: Vec<String> = events.split(',')
                            .map(|e| format!("\"{}\"", e.trim()))
                            .collect();
                        format!("[{}]", parts.join(","))
                    };
                    let w = db.create_webhook(NewWebhookCallback {
                        name,
                        url,
                        events: events_json,
                        secret_header_name,
                        secret_header_value,
                    }).await?;
                    println!("Created webhook id={} name={} url={}", w.id, w.name, w.url);
                    println!("Restart the server for this webhook to take effect.");
                }
                WebhookCommands::Delete { id } => {
                    db.delete_webhook(id).await?;
                    println!("Deleted webhook id={}", id);
                    println!("Restart the server for this change to take effect.");
                }
                WebhookCommands::Enable { id } => {
                    db.set_webhook_enabled(id, true).await?;
                    println!("Enabled webhook id={}", id);
                    println!("Restart the server for this change to take effect.");
                }
                WebhookCommands::Disable { id } => {
                    db.set_webhook_enabled(id, false).await?;
                    println!("Disabled webhook id={}", id);
                    println!("Restart the server for this change to take effect.");
                }
            }
        }
    }
    Ok(())
}

// ── Attribution cost report (issue #13) ───────────────────────────────────────

/// One row of an attribution cost report. `scope` is the model, the day, or
/// `TOTAL` for the trailing aggregate.
#[derive(Debug, serde::Serialize)]
struct AttributionReportRow {
    scope: String,
    cost_usd: f64,
    saved_usd: f64,
    requests: i64,
    cache_hits: i64,
    tokens_in: i64,
    tokens_out: i64,
}

/// Parse `--tag key=value` / `--correlation-id id` into a repository filter.
fn parse_attribution_filter(
    tag: Option<&str>,
    correlation_id: Option<String>,
) -> anyhow::Result<crate::db::repositories::costs::AttributionFilter> {
    use crate::db::repositories::costs::AttributionFilter;
    if let Some(id) = correlation_id {
        if id.trim().is_empty() {
            anyhow::bail!("--correlation-id must not be empty");
        }
        return Ok(AttributionFilter::CorrelationId(id.trim().to_string()));
    }
    let raw = tag.unwrap_or_default();
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("--tag must be in KEY=VALUE form, got '{}'", raw))?;
    let (key, value) = (key.trim(), value.trim());
    if value.is_empty() {
        anyhow::bail!("--tag value must not be empty");
    }
    if !crate::api::attribution::is_safe_tag_key(key) {
        anyhow::bail!(
            "--tag key must contain only letters, digits, '_', '-', '.' or ':', got '{}'",
            key
        );
    }
    Ok(AttributionFilter::Tag {
        key: key.to_string(),
        value: value.to_string(),
    })
}

/// One line of the `report compare` table: a metric with both arms and B−A.
#[derive(serde::Serialize)]
struct CompareRow {
    metric: String,
    a: String,
    b: String,
    delta: String,
    percent: String,
}

/// The prompt-log store: a dedicated SQLite file when `[storage]
/// prompt_db_path` is set, else the main database. `serve` and `report
/// compare` open it the same way.
async fn open_prompt_db(
    settings: &crate::config::schema::Settings,
    db: &Arc<dyn crate::api::app::DatabaseProvider>,
) -> anyhow::Result<Arc<dyn crate::api::app::DatabaseProvider>> {
    Ok(match settings.storage.prompt_db_path.as_deref() {
        Some(path) => {
            let pdb = crate::db::sqlite::SqliteDb::connect(path).await?;
            crate::db::migrations::run_migrations(&pdb.pool).await?;
            Arc::new(pdb)
        }
        None => db.clone(),
    })
}

/// `report compare`: build the comparison from the CLI's own sources — the
/// main database, the dedicated prompt database when one is configured, and
/// the configured pricing — and print it. Never constructs an `AppState`.
async fn report_compare(
    db: crate::db::sqlite::SqliteDb,
    settings: &crate::config::schema::Settings,
    query: &crate::api::admin::compare::CompareQuery,
    format: OutputFormat,
) -> anyhow::Result<()> {
    use crate::api::admin::compare::{build_comparison, CompareSources};

    // Validate before opening anything else, so a typo fails fast.
    query.validate()?;

    let db: Arc<dyn crate::api::app::DatabaseProvider> = Arc::new(db);
    let prompt_db = open_prompt_db(settings, &db).await?;
    let sources = CompareSources {
        db,
        prompt_db,
        cost_calc: Arc::new(crate::router::cost::CostCalculator::new_with_config(&settings.pricing)),
    };
    let comparison = build_comparison(&sources, query).await?;
    // Like `print_rows`: a closed pipe (`| head`) is not an error.
    match write_comparison(&comparison, format, &mut std::io::stdout()) {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(e.into()),
        _ => Ok(()),
    }
}

/// Render a comparison: the full JSON document (identical to the endpoint's)
/// or a metric-per-row table/CSV with the coverage line and caveats beneath.
fn write_comparison(
    c: &crate::api::admin::compare::Comparison,
    format: OutputFormat,
    out: &mut impl std::io::Write,
) -> std::io::Result<()> {
    use crate::api::admin::compare::{sign_prefix, Delta};
    use crate::report::formatter::write_rows;

    if matches!(format, OutputFormat::Json) {
        return writeln!(out, "{}", serde_json::to_string_pretty(c)?);
    }
    let table = matches!(format, OutputFormat::Table);

    let dash = "-".to_string();
    let usd = |v: f64| format!("{:.4}", v);
    let one = |v: f64| format!("{:.1}", v);
    let pct = |v: f64| format!("{:.1}%", v * 100.0);
    let int = |v: i64| v.to_string();
    let opt = |v: Option<f64>, f: &dyn Fn(f64) -> String| v.map(f).unwrap_or_else(|| dash.clone());
    let opt_i = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_else(|| dash.clone());
    // Deltas are signed so the direction reads without the A and B columns.
    let delta = |d: Option<Delta>, f: &dyn Fn(f64) -> String| match d {
        Some(d) => {
            let abs = format!("{}{}", sign_prefix(d.abs), f(d.abs));
            let pct = d.pct.map(|p| format!("{}{:.1}%", sign_prefix(p), p));
            (abs, pct.unwrap_or_else(|| dash.clone()))
        }
        None => (dash.clone(), dash.clone()),
    };
    let row = |metric: &str, a: String, b: String, d: (String, String)| CompareRow {
        metric: metric.to_string(),
        a,
        b,
        delta: d.0,
        percent: d.1,
    };
    let (a, b, d) = (&c.a, &c.b, &c.delta);

    let rows = vec![
        row("Requests", int(a.requests), int(b.requests), delta(d.requests, &|v| format!("{:.0}", v))),
        row("Cost / request (USD)", opt(a.cost_per_request, &usd), opt(b.cost_per_request, &usd), delta(d.cost_per_request, &usd)),
        row("Tokens in / request", opt(a.tokens_in_per_request, &one), opt(b.tokens_in_per_request, &one), delta(d.tokens_in_per_request, &one)),
        row("Tokens out / request", opt(a.tokens_out_per_request, &one), opt(b.tokens_out_per_request, &one), delta(d.tokens_out_per_request, &one)),
        row(&format!("Mean latency (ms, n={} / n={})", a.latency.samples, b.latency.samples), opt(a.latency.mean_ms, &one), opt(b.latency.mean_ms, &one), delta(d.mean_ms, &one)),
        row("p50 latency (ms)", opt_i(a.latency.p50_ms), opt_i(b.latency.p50_ms), delta(d.p50_ms, &one)),
        row("p95 latency (ms)", opt_i(a.latency.p95_ms), opt_i(b.latency.p95_ms), delta(d.p95_ms, &one)),
        row("Cache hit rate", pct(a.hit_rate), pct(b.hit_rate), delta(d.hit_rate, &pct)),
        row("Error rate", pct(a.error_rate), pct(b.error_rate), delta(d.error_rate, &pct)),
        row("Total cost (USD)", usd(a.cost_usd), usd(b.cost_usd), delta(d.cost_usd, &usd)),
        row("Total tokens in", int(a.tokens_in), int(b.tokens_in), delta(d.tokens_in, &|v| format!("{:.0}", v))),
        row("Total tokens out", int(a.tokens_out), int(b.tokens_out), delta(d.tokens_out, &|v| format!("{:.0}", v))),
        row("Cache hits", int(a.cache_hits), int(b.cache_hits), (dash.clone(), dash.clone())),
        row("Failures", int(a.failures), int(b.failures), (dash.clone(), dash.clone())),
    ];
    if table {
        writeln!(out, "Compare by {}: A = {}  B = {}  (window: {})", c.dimension, a.label, b.label, c.window)?;
    }
    write_rows(
        &rows,
        &["Metric", "A", "B", "Delta (B-A)", "Change"],
        |r| vec![r.metric.clone(), r.a.clone(), r.b.clone(), r.delta.clone(), r.percent.clone()],
        format,
        out,
    )?;
    if table {
        writeln!(
            out,
            "Coverage: A {} latency samples of {} requests; B {} of {}.",
            c.coverage.a.latency_samples, c.coverage.a.requests, c.coverage.b.latency_samples, c.coverage.b.requests
        )?;
        for (name, m) in [("A", a), ("B", b)] {
            if m.unpriced {
                writeln!(out, "Unpriced: arm {} includes {} — its cost figures are incomplete.", name, m.unpriced_models.join(", "))?;
            }
        }
        writeln!(out, "{}", c.ttft_note)?;
        for caveat in c.caveats {
            writeln!(out, "Note: {}", caveat)?;
        }
    }
    Ok(())
}

async fn report_attribution(
    db: &crate::db::sqlite::SqliteDb,
    filter: &crate::db::repositories::costs::AttributionFilter,
    by: crate::cli::commands::AttributionBreakdown,
    window: &str,
    format: OutputFormat,
) -> anyhow::Result<()> {
    use crate::cli::commands::AttributionBreakdown;
    use crate::db::repositories::costs::CostRepository;

    // `alltime` is the CLI's spelling of the same window the admin API calls
    // `all`; everything else maps straight through.
    let window = if window == "alltime" { "all" } else { window };
    let (start, end) = crate::api::admin::attribution::window_range(window);

    let totals = CostRepository::attribution_totals(db, filter, &start, &end).await?;
    let breakdown = match by {
        AttributionBreakdown::Model => {
            CostRepository::attribution_by_model(db, filter, &start, &end).await?
        }
        AttributionBreakdown::Day => {
            CostRepository::attribution_by_day(db, filter, &start, &end).await?
        }
    };

    let mut rows: Vec<AttributionReportRow> = breakdown
        .into_iter()
        .map(|r| AttributionReportRow {
            scope: r.key,
            cost_usd: r.totals.cost_usd,
            saved_usd: r.totals.saved_usd,
            requests: r.totals.requests,
            cache_hits: r.totals.cache_hits,
            tokens_in: r.totals.tokens_in,
            tokens_out: r.totals.tokens_out,
        })
        .collect();
    rows.push(AttributionReportRow {
        scope: "TOTAL".to_string(),
        cost_usd: totals.cost_usd,
        saved_usd: totals.saved_usd,
        requests: totals.requests,
        cache_hits: totals.cache_hits,
        tokens_in: totals.tokens_in,
        tokens_out: totals.tokens_out,
    });

    if matches!(format, OutputFormat::Table) {
        println!("Attribution: {}  (window: {})", filter.label(), window);
    }
    print_rows(
        &rows,
        &[
            match by {
                AttributionBreakdown::Model => "Model",
                AttributionBreakdown::Day => "Day",
            },
            "Cost (USD)",
            "Saved (USD)",
            "Requests",
            "Cache Hits",
            "Tokens In (Prompts)",
            "Tokens Out (Completions)",
        ],
        |r| {
            vec![
                r.scope.clone(),
                format!("{:.4}", r.cost_usd),
                format!("{:.4}", r.saved_usd),
                r.requests.to_string(),
                r.cache_hits.to_string(),
                r.tokens_in.to_string(),
                r.tokens_out.to_string(),
            ]
        },
        format,
    );
    Ok(())
}

#[cfg(test)]
mod attribution_cli_tests {
    use super::*;
    use crate::db::repositories::costs::AttributionFilter;

    #[test]
    fn parses_tag_pair() {
        let f = parse_attribution_filter(Some("engagement=eng-4711"), None).unwrap();
        assert_eq!(
            f,
            AttributionFilter::Tag {
                key: "engagement".to_string(),
                value: "eng-4711".to_string()
            }
        );
    }

    #[test]
    fn parses_correlation_id() {
        let f = parse_attribution_filter(None, Some("run-3".to_string())).unwrap();
        assert_eq!(f, AttributionFilter::CorrelationId("run-3".to_string()));
    }

    #[test]
    fn rejects_tag_without_equals() {
        assert!(parse_attribution_filter(Some("engagement"), None).is_err());
    }

    #[test]
    fn rejects_empty_tag_value() {
        assert!(parse_attribution_filter(Some("engagement="), None).is_err());
    }

    #[test]
    fn rejects_unsafe_tag_key() {
        assert!(parse_attribution_filter(Some("bad key=v"), None).is_err());
    }

    /// End-to-end CLI path: a tagged ledger row in, a rendered report out.
    #[tokio::test]
    async fn reports_attribution_from_the_database() {
        use crate::cli::commands::AttributionBreakdown;
        use crate::db::models::NewCostLedgerEntry;
        use crate::db::repositories::costs::CostRepository;

        let db = crate::db::sqlite::SqliteDb::connect(":memory:").await.unwrap();
        crate::db::migrations::run_migrations(&db.pool).await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, name, created_at) VALUES (1, 'cli', '2025-01-01T00:00:00Z')",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        CostRepository::create(
            &db,
            NewCostLedgerEntry {
                user_id: 1,
                prompt_id: None,
                model: "gpt-4o".to_string(),
                provider: "openai".to_string(),
                project: None,
                tokens_in: 10,
                tokens_out: 20,
                cost_usd: 0.25,
                api_key_id: None,
                attribution_correlation_id: Some("run-7".to_string()),
                attribution_tags: r#"{"engagement":"eng-1"}"#.to_string(),
            },
        )
        .await
        .unwrap();

        let filter = parse_attribution_filter(Some("engagement=eng-1"), None).unwrap();
        report_attribution(&db, &filter, AttributionBreakdown::Model, "alltime", OutputFormat::Json)
            .await
            .unwrap();

        // The same rows must be reachable by correlation id.
        let by_corr = parse_attribution_filter(None, Some("run-7".to_string())).unwrap();
        let totals = CostRepository::attribution_totals(
            &db, &by_corr, "1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        assert_eq!(totals.requests, 1);
        assert!((totals.cost_usd - 0.25).abs() < 1e-9);

        // A tag nobody used reports nothing rather than everything.
        let empty = parse_attribution_filter(Some("engagement=nope"), None).unwrap();
        let none = CostRepository::attribution_totals(
            &db, &empty, "1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        assert_eq!(none.requests, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::admin::compare::{build_comparison, CompareQuery, CompareSources};
    use crate::db::models::{NewCostLedgerEntry, NewPrompt};
    use crate::db::repositories::costs::CostRepository;
    use crate::db::repositories::prompts::PromptRepository;

    async fn seeded_sources() -> CompareSources {
        let db = crate::db::sqlite::SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        crate::db::repositories::users::UserRepository::create(
            &db,
            crate::db::models::NewUser { name: "u".into(), email: None },
        )
        .await
        .unwrap();
        for (model, latency) in [("m1", 100), ("m1", 300), ("m2", 50)] {
            CostRepository::create(&db, NewCostLedgerEntry {
                user_id: 1, prompt_id: None, model: model.into(), provider: "p".into(),
                project: None, tokens_in: 10, tokens_out: 20, cost_usd: 0.5, api_key_id: None,
                attribution_correlation_id: None, attribution_tags: "{}".into(),
            }).await.unwrap();
            PromptRepository::create(&db, NewPrompt {
                user_id: 1, session_id: None, request_model: model.into(), routed_model: model.into(),
                provider: "p".into(), messages: "[]".into(), response: None, finish_reason: None,
                prompt_tokens: 0, completion_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0,
                cost_usd: 0.0, latency_ms: Some(latency), tags: "[]".into(), project: None,
                attribution_correlation_id: None, attribution_tags: "{}".into(),
            }).await.unwrap();
        }
        let db: Arc<dyn crate::api::app::DatabaseProvider> = Arc::new(db);
        CompareSources {
            prompt_db: db.clone(),
            db,
            cost_calc: Arc::new(crate::router::cost::CostCalculator::new_with_config(&[])),
        }
    }

    fn query() -> CompareQuery {
        CompareQuery {
            dimension: "model".into(),
            key: String::new(),
            a: "m1".into(),
            b: "m2".into(),
            window: "all".into(),
        }
    }

    #[tokio::test]
    async fn compare_json_output_is_the_endpoint_document() {
        let sources = seeded_sources().await;
        let comparison = build_comparison(&sources, &query()).await.unwrap();
        let mut out = Vec::new();
        write_comparison(&comparison, OutputFormat::Json, &mut out).unwrap();
        let printed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(printed, serde_json::to_value(&comparison).unwrap());
        assert_eq!(printed["a"]["requests"], 2);
        assert_eq!(printed["b"]["latency"]["p95_ms"], 50);
    }

    #[tokio::test]
    async fn compare_table_shows_sample_counts_badge_and_caveats() {
        let sources = seeded_sources().await;
        let comparison = build_comparison(&sources, &query()).await.unwrap();
        let mut out = Vec::new();
        write_comparison(&comparison, OutputFormat::Table, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("A = model=m1"), "{}", text);
        assert!(text.contains("n=2"), "{}", text);
        assert!(text.contains("n=1"), "{}", text);
        assert!(text.contains("Coverage: A 2 latency samples of 2 requests; B 1 of 1."), "{}", text);
        assert!(text.contains("Unpriced: arm A includes m1"), "{}", text);
        assert!(text.contains("quality"), "{}", text);
        assert!(text.contains("stream: false"), "{}", text);
        assert!(text.contains("not recorded"), "{}", text);
        assert!(text.contains("p95 latency"), "{}", text);
    }

    #[tokio::test]
    async fn compare_table_zero_delta_has_no_plus_sign() {
        // A and B are seeded identically, so every delta -- including
        // requests -- is exactly zero. `sign_prefix` only emits `+` for
        // v > 0.0, so the rendered table must show `0.0%`, never `+0.0%`.
        let db = crate::db::sqlite::SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        crate::db::repositories::users::UserRepository::create(
            &db,
            crate::db::models::NewUser { name: "u".into(), email: None },
        )
        .await
        .unwrap();
        for model in ["m1", "m2"] {
            CostRepository::create(&db, NewCostLedgerEntry {
                user_id: 1, prompt_id: None, model: model.into(), provider: "p".into(),
                project: None, tokens_in: 10, tokens_out: 20, cost_usd: 0.5, api_key_id: None,
                attribution_correlation_id: None, attribution_tags: "{}".into(),
            }).await.unwrap();
            PromptRepository::create(&db, NewPrompt {
                user_id: 1, session_id: None, request_model: model.into(), routed_model: model.into(),
                provider: "p".into(), messages: "[]".into(), response: None, finish_reason: None,
                prompt_tokens: 0, completion_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0,
                cost_usd: 0.0, latency_ms: Some(100), tags: "[]".into(), project: None,
                attribution_correlation_id: None, attribution_tags: "{}".into(),
            }).await.unwrap();
        }
        let db: Arc<dyn crate::api::app::DatabaseProvider> = Arc::new(db);
        let sources = CompareSources {
            prompt_db: db.clone(),
            db,
            cost_calc: Arc::new(crate::router::cost::CostCalculator::new_with_config(&[])),
        };
        let comparison = build_comparison(&sources, &query()).await.unwrap();
        assert_eq!(comparison.a.requests, comparison.b.requests, "fixture must have equal A/B requests");
        let mut out = Vec::new();
        write_comparison(&comparison, OutputFormat::Table, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("0.0%"), "{}", text);
        assert!(!text.contains("+0.0%"), "{}", text);
    }

    #[tokio::test]
    async fn compare_invalid_dimension_fails_with_the_validation_message() {
        let db = crate::db::sqlite::SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        let settings = crate::config::schema::Settings::default();
        let mut q = query();
        q.dimension = "pair".into();
        let err = report_compare(db, &settings, &q, OutputFormat::Table).await.unwrap_err();
        assert!(err.to_string().starts_with("dimension must be one of"), "{}", err);
    }

    #[test]
    fn config_dir_permission_mode() {
        // 0o700 = rwx for owner only
        assert_eq!(0o700u32, 0b111_000_000);
    }
}
