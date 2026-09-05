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

            // Validate OIDC role against the known vocabulary {"superadmin", "viewer"}.
            // An unknown role would silently degrade to viewer in session extractors,
            // so this ensures operators set an explicit valid role if they want SSO
            // admins to hold superadmin (issue #51).
            if settings.oidc.enabled {
                settings.oidc.validate_role()?;
            }

            // Bootstrap admin account from config if specified (issue #43).
            if let Some(ref bootstrap) = settings.admin.bootstrap {
                bootstrap.apply(&*db).await?;
            }

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

            // Prompt-log retention: an hourly tick against the LIVE policy, so
            // a retention set in the GUI applies within the hour.
            // retention_days == 0 (the default) means keep forever — deletion
            // is strictly opt-in. The rows of a retaining experiment are
            // exempt while its content window is open, and once it has
            // elapsed they are redacted in place (spec §7c); that half runs
            // on every tick whatever the global retention says. The
            // experiment list is read from the main database, the prompt rows
            // live in the prompt store. Failures are logged, never fatal.
            {
                let tick_db = db.clone();
                let tick_prompt_db = prompt_db.clone();
                let tick_storage = storage_live.clone();
                tokio::spawn(async move {
                    loop {
                        let retention_days = tick_storage.load().prompt_retention_days;
                        crate::db::retention::run_retention_tick(
                            &*tick_db,
                            &*tick_db,
                            &*tick_prompt_db,
                            retention_days,
                            chrono::Utc::now(),
                        )
                        .await;
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

            // Experiment registry (spec §7a): load once before serving so the
            // first request can bind; the tick below keeps it fresh.
            let experiments = Arc::new(crate::router::experiments::ExperimentRegistry::default());
            match experiments.load_from(&*db).await {
                Ok(()) if !experiments.is_empty() => {
                    tracing::info!(count = experiments.len(), "loaded experiments")
                }
                Ok(()) => {}
                Err(e) => tracing::warn!(error = %e, "experiment registry load failed"),
            }

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
                experiments,
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

            // Experiment lifecycle tick (spec §7a): every minute close the
            // experiments whose `expires_at` has passed, audit each close as
            // actor `system`, then reload the registry so admin writes made
            // from another process are picked up too. Binding already refuses
            // an expired experiment per request, so this is bookkeeping, not
            // enforcement. Failures are logged, never fatal.
            {
                let tick_db = state.db.clone();
                let tick_registry = state.experiments.clone();
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
                    loop {
                        interval.tick().await;
                        let now = chrono::Utc::now();
                        match tick_db.close_expired(now.timestamp(), &now.to_rfc3339()).await {
                            Ok(ids) => {
                                for id in ids {
                                    tracing::info!(experiment_id = id, "experiment auto-closed on expiry");
                                    crate::api::admin::audit::audit(
                                        &tick_db,
                                        None,
                                        "system",
                                        "experiment.close",
                                        Some(id.to_string()),
                                        None,
                                        Some(serde_json::json!({"status": "closed", "reason": "expired"}).to_string()),
                                    )
                                    .await;
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "experiment auto-close failed"),
                        }
                        if let Err(e) = tick_registry.load_from(&*tick_db).await {
                            tracing::warn!(error = %e, "experiment registry reload failed");
                        }
                    }
                });
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
        Commands::Experiment(experiment_args) => {
            let settings = crate::config::load(cli.config)?;
            let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
            crate::db::migrations::run_migrations(&db.pool).await?;
            let db: Arc<dyn crate::api::app::DatabaseProvider> = Arc::new(db);
            run_experiment_command(&db, &settings, experiment_args.command).await?;
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
        if let Some(exp) = &c.experiment {
            writeln!(
                out,
                "Experiment: {} (#{}, {}{})",
                exp.name,
                exp.id,
                exp.status.as_str(),
                if exp.retain_content { ", retains content" } else { "" }
            )?;
        }
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
        if let Some(exp) = &c.experiment {
            writeln!(out, "Note: {}", exp.stored_content_note)?;
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
                experiment_id: None,
                experiment_variant: None,
                tokens_estimated: false,
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
                experiment_id: None,
                experiment_variant: None,
                tokens_estimated: false,
            }).await.unwrap();
            PromptRepository::create(&db, NewPrompt {
                user_id: 1, session_id: None, request_model: model.into(), routed_model: model.into(),
                provider: "p".into(), messages: "[]".into(), response: None, finish_reason: None,
                prompt_tokens: 0, completion_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0,
                cost_usd: 0.0, latency_ms: Some(latency), tags: "[]".into(), project: None,
                attribution_correlation_id: None, attribution_tags: "{}".into(),
                experiment_id: None,
                experiment_variant: None,
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
                experiment_id: None,
                experiment_variant: None,
                tokens_estimated: false,
            }).await.unwrap();
            PromptRepository::create(&db, NewPrompt {
                user_id: 1, session_id: None, request_model: model.into(), routed_model: model.into(),
                provider: "p".into(), messages: "[]".into(), response: None, finish_reason: None,
                prompt_tokens: 0, completion_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0,
                cost_usd: 0.0, latency_ms: Some(100), tags: "[]".into(), project: None,
                attribution_correlation_id: None, attribution_tags: "{}".into(),
                experiment_id: None,
                experiment_variant: None,
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

    /// An experiment with two variants and stamped ledger rows on each arm.
    async fn seeded_variant_sources() -> (CompareSources, i64) {
        use crate::db::repositories::experiments::ExperimentRepository;
        let db = crate::db::sqlite::SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        crate::db::repositories::users::UserRepository::create(
            &db,
            crate::db::models::NewUser { name: "u".into(), email: None },
        )
        .await
        .unwrap();
        let mut variants: crate::db::models::ExperimentVariants = Default::default();
        variants.insert("control".into(), Default::default());
        variants.insert("candidate".into(), Default::default());
        let exp = ExperimentRepository::create(&db, crate::db::models::NewExperiment {
            name: "exp".into(),
            variants,
            allowed_user_ids: vec![],
            feed_learning: false,
            expires_at: 4_102_444_800,
            retain_content: true,
            content_retention_days: 0,
        })
        .await
        .unwrap();
        for (variant, latency) in [("control", 100), ("control", 300), ("candidate", 50)] {
            CostRepository::create(&db, NewCostLedgerEntry {
                user_id: 1, prompt_id: None, model: "m".into(), provider: "p".into(),
                project: None, tokens_in: 10, tokens_out: 20, cost_usd: 0.5, api_key_id: None,
                attribution_correlation_id: None, attribution_tags: "{}".into(),
                experiment_id: Some(exp.id),
                experiment_variant: Some(variant.into()),
                tokens_estimated: false,
            }).await.unwrap();
            PromptRepository::create(&db, NewPrompt {
                user_id: 1, session_id: None, request_model: "m".into(), routed_model: "m".into(),
                provider: "p".into(), messages: "[]".into(), response: None, finish_reason: None,
                prompt_tokens: 0, completion_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0,
                cost_usd: 0.0, latency_ms: Some(latency), tags: "[]".into(), project: None,
                attribution_correlation_id: None, attribution_tags: "{}".into(),
                experiment_id: Some(exp.id),
                experiment_variant: Some(variant.into()),
            }).await.unwrap();
        }
        let db: Arc<dyn crate::api::app::DatabaseProvider> = Arc::new(db);
        let sources = CompareSources {
            prompt_db: db.clone(),
            db,
            cost_calc: Arc::new(crate::router::cost::CostCalculator::new_with_config(&[])),
        };
        (sources, exp.id)
    }

    fn variant_query(experiment: i64) -> CompareQuery {
        CompareQuery {
            dimension: "variant".into(),
            key: experiment.to_string(),
            a: "control".into(),
            b: "candidate".into(),
            window: "all".into(),
        }
    }

    #[tokio::test]
    async fn compare_variant_json_carries_the_arms_and_the_experiment() {
        let (sources, exp) = seeded_variant_sources().await;
        let comparison = build_comparison(&sources, &variant_query(exp)).await.unwrap();
        let mut out = Vec::new();
        write_comparison(&comparison, OutputFormat::Json, &mut out).unwrap();
        let printed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(printed["dimension"], "variant");
        assert_eq!(printed["key"], exp.to_string());
        assert_eq!(printed["a"]["label"], format!("experiment={exp}:control"));
        assert_eq!(printed["a"]["requests"], 2);
        assert_eq!(printed["a"]["latency"]["p95_ms"], 300);
        assert_eq!(printed["b"]["label"], format!("experiment={exp}:candidate"));
        assert_eq!(printed["b"]["requests"], 1);
        assert_eq!(printed["experiment"]["name"], "exp");
        assert_eq!(printed["experiment"]["retain_content"], true);
        assert!(printed["caveats"][0].as_str().unwrap().contains("/admin/experiments"));
    }

    #[tokio::test]
    async fn compare_variant_csv_and_table_carry_the_arms() {
        let (sources, exp) = seeded_variant_sources().await;
        let comparison = build_comparison(&sources, &variant_query(exp)).await.unwrap();

        let mut out = Vec::new();
        write_comparison(&comparison, OutputFormat::Csv, &mut out).unwrap();
        let csv = String::from_utf8(out).unwrap();
        let requests = csv.lines().find(|l| l.starts_with("Requests")).unwrap();
        assert_eq!(requests, "Requests,2,1,-1,-50.0%", "{csv}");
        assert!(csv.lines().any(|l| l.starts_with("p95 latency (ms),300,50,")), "{csv}");
        assert!(!csv.contains("Note:"), "csv must be rows only: {csv}");

        let mut out = Vec::new();
        write_comparison(&comparison, OutputFormat::Table, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(&format!("Compare by variant: A = experiment={exp}:control  B = experiment={exp}:candidate")), "{text}");
        assert!(text.contains(&format!("Experiment: exp (#{exp}, active, retains content)")), "{text}");
        assert!(text.contains("never purged"), "{text}");
        assert!(text.contains("/admin/experiments"), "{text}");
    }

    #[tokio::test]
    async fn compare_variant_rejects_an_undeclared_label_and_an_unknown_experiment() {
        let (sources, exp) = seeded_variant_sources().await;
        let mut q = variant_query(exp);
        q.b = "nope".into();
        let err = build_comparison(&sources, &q).await.unwrap_err().to_string();
        assert!(err.starts_with("b: ") && err.contains("nope"), "{err}");
        let q = variant_query(exp + 1);
        let err = build_comparison(&sources, &q).await.unwrap_err().to_string();
        assert!(err.contains(&(exp + 1).to_string()), "{err}");
    }

    #[tokio::test]
    async fn compare_variant_with_a_bad_key_fails_before_opening_the_prompt_database() {
        let db = crate::db::sqlite::SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        let settings = crate::config::schema::Settings::default();
        let mut q = variant_query(1);
        q.key = "one".into();
        let err = report_compare(db, &settings, &q, OutputFormat::Table).await.unwrap_err();
        assert!(err.to_string().starts_with("key must be an experiment id"), "{}", err);
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

// ── Experiments (spec §7a) ────────────────────────────────────────────────────
//
// `experiment add|list|close|results`: direct database writes like `alias`
// and `webhook`, each audited as actor `cli`. Creation runs the same body
// validation and the same pricing gate as `POST /admin/api/experiments`,
// with the gate's inputs built from settings — the alias-aware router, the
// pool map and the price table — so no provider adapter is constructed and
// no credential is needed. A running server reloads its registry every 60
// seconds, so a write made here is honoured within a minute.

/// Dispatch one `experiment` subcommand against an open database.
async fn run_experiment_command(
    db: &Arc<dyn crate::api::app::DatabaseProvider>,
    settings: &crate::config::schema::Settings,
    command: commands::ExperimentCommands,
) -> anyhow::Result<()> {
    use commands::ExperimentCommands;
    match command {
        ExperimentCommands::Add(args) => {
            let row = experiment_add(db, settings, &args).await?;
            println!(
                "Created experiment id={} name={} (expires {}, retain content {})",
                row.id,
                row.name,
                render_expires_at(row.expires_at),
                render_retention(&row),
            );
            for (label, overlay) in &row.variants {
                let targets: Vec<String> = overlay
                    .iter()
                    .map(|(key, t)| format!("{key} -> {}/{}", t.provider, t.model))
                    .collect();
                if targets.is_empty() {
                    println!("  {label}: (no overlay)");
                } else {
                    println!("  {label}: {}", targets.join(", "));
                }
            }
            println!("A running server picks up this experiment within 60 seconds.");
        }
        ExperimentCommands::List { status, format } => {
            let rows = experiment_list(db, &status).await?;
            print_rows(&rows, EXPERIMENT_LIST_HEADERS, experiment_list_row, format);
        }
        ExperimentCommands::Close { id } => {
            let row = experiment_close(db, id).await?;
            println!(
                "Closed experiment id={} name={} at {}",
                row.id,
                row.name,
                row.closed_at.as_deref().unwrap_or("-"),
            );
            println!("A running server stops binding to it within 60 seconds.");
        }
        ExperimentCommands::Results { id, limit, offset, format } => {
            use crate::api::admin::experiments::RunPage;
            // The page bounds and their messages are the endpoint's.
            let (limit, offset) = (limit.map(|n| n.to_string()), offset.map(|n| n.to_string()));
            let page = RunPage::parse(limit.as_deref(), offset.as_deref())
                .map_err(anyhow::Error::msg)?;
            let results = experiment_results(db, settings, id, page).await?;
            // Like `print_rows`: a closed pipe (`| head`) is not an error.
            match write_experiment_results(&results, format, &mut std::io::stdout()) {
                Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => return Err(e.into()),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Parse the repeated `--variant LABEL=KEY:TARGET[,KEY:TARGET...]` flags into
/// the `variants` object of the API body. Only the flag grammar is checked
/// here; labels, bounds and targets are validated by the shared
/// `parse_create` and gate, so the CLI refuses what the API refuses, in the
/// same words.
fn parse_variant_flags(flags: &[String]) -> anyhow::Result<serde_json::Value> {
    let mut variants = serde_json::Map::new();
    for flag in flags {
        let (label, overlay) = flag.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("--variant must be LABEL=KEY:TARGET[,KEY:TARGET...], got '{flag}'")
        })?;
        let label = label.trim();
        let mut entries = serde_json::Map::new();
        for entry in overlay.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (key, target) = entry.split_once(':').ok_or_else(|| {
                anyhow::anyhow!("--variant {label}: entry '{entry}' must be KEY:TARGET")
            })?;
            let (key, target) = (key.trim(), target.trim());
            if entries.insert(key.to_string(), serde_json::json!(target)).is_some() {
                anyhow::bail!("--variant {label}: key '{key}' appears more than once");
            }
        }
        if variants
            .insert(label.to_string(), serde_json::Value::Object(entries))
            .is_some()
        {
            // A JSON object cannot say this; the API's wording for the case.
            anyhow::bail!("variants: label '{label}' appears more than once");
        }
    }
    Ok(serde_json::Value::Object(variants))
}

/// The body `POST /admin/api/experiments` would receive for these flags.
/// `never` is the CLI spelling of the API's `0`; anything else is passed
/// through as a string for `parse_create` to validate.
fn experiment_create_body(
    args: &commands::ExperimentAddArgs,
    variants: serde_json::Value,
    allowed_user_ids: Vec<i64>,
) -> serde_json::Value {
    let expires_at = args.expires_at.trim();
    let expires_at = if expires_at.eq_ignore_ascii_case("never") {
        serde_json::json!(0)
    } else {
        serde_json::json!(expires_at)
    };
    serde_json::json!({
        "name": args.name,
        "variants": variants,
        "expires_at": expires_at,
        "content_retention_days": args.content_retention_days,
        "retain_content": args.retain_content,
        "feed_learning": args.feed_learning,
        "allowed_user_ids": allowed_user_ids,
    })
}

/// `experiment add`: validate, gate, store and audit, returning the row.
async fn experiment_add(
    db: &Arc<dyn crate::api::app::DatabaseProvider>,
    settings: &crate::config::schema::Settings,
    args: &commands::ExperimentAddArgs,
) -> anyhow::Result<crate::db::models::Experiment> {
    use crate::api::admin::experiments::{
        audit_row, gate_variants, is_unique_violation, parse_create, GateSources,
    };
    use crate::db::repositories::experiments::ExperimentRepository;
    use crate::db::repositories::users::UserRepository;

    let variants = parse_variant_flags(&args.variants)?;
    let mut allowed_user_ids: Vec<i64> = Vec::new();
    for name in &args.allow_users {
        let user = UserRepository::find_by_name(&**db, name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("--allow-user: no user named '{name}'"))?;
        if !allowed_user_ids.contains(&user.id) {
            allowed_user_ids.push(user.id);
        }
    }
    let body = experiment_create_body(args, variants, allowed_user_ids);
    let parsed = parse_create(&body, chrono::Utc::now()).map_err(anyhow::Error::msg)?;

    // The creation gate from settings alone: config and DB aliases resolve
    // through the same router the server uses, pools come from
    // `[routing.load_balancer]`, prices from `[[pricing]]` over the built-in
    // table, and a provider counts as configured when `[providers.<name>]`
    // exists. No adapter is built, so no credential is read.
    let router = crate::router::engine::RequestRouter::new(Arc::new(settings.clone()));
    router.update_db_aliases(crate::api::admin::aliases::build_db_alias_map(db).await);
    let load_balancer =
        crate::router::load_balancer::LoadBalancer::new(settings.routing.load_balancer.clone());
    let cost_calc = crate::router::cost::CostCalculator::new_with_config(&settings.pricing);
    let gate = GateSources {
        router: &router,
        load_balancer: &load_balancer,
        has_provider: Box::new(|name| settings.providers.contains_key(name)),
        cost_calc: &cost_calc,
    };
    let variants = gate_variants(&gate, &parsed.variants).map_err(anyhow::Error::msg)?;

    let name = parsed.name;
    let row = ExperimentRepository::create(
        &**db,
        crate::db::models::NewExperiment {
            name: name.clone(),
            variants,
            allowed_user_ids: parsed.allowed_user_ids,
            feed_learning: parsed.feed_learning,
            expires_at: parsed.expires_at,
            retain_content: parsed.retain_content,
            content_retention_days: parsed.content_retention_days,
        },
    )
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            anyhow::anyhow!("name '{name}' is already taken")
        } else {
            e
        }
    })?;

    admin::audit_change(
        &**db,
        "experiment.create",
        &format!("experiment:{}", row.id),
        None,
        audit_row(&row),
    )
    .await;
    Ok(row)
}

/// `experiment list --status`: the same filter words as the endpoint.
async fn experiment_list(
    db: &Arc<dyn crate::api::app::DatabaseProvider>,
    status: &str,
) -> anyhow::Result<Vec<crate::db::models::Experiment>> {
    use crate::db::repositories::experiments::{ExperimentRepository, ExperimentStatusFilter};
    let filter = match status.trim() {
        "" | "active" => ExperimentStatusFilter::Active,
        "closed" => ExperimentStatusFilter::Closed,
        "all" => ExperimentStatusFilter::All,
        other => anyhow::bail!("status must be active, closed or all, got '{other}'"),
    };
    ExperimentRepository::list(&**db, filter).await
}

/// `experiment close --id`: the endpoint's semantics — a missing id and a
/// second close are errors — audited with the closed row.
async fn experiment_close(
    db: &Arc<dyn crate::api::app::DatabaseProvider>,
    id: i64,
) -> anyhow::Result<crate::db::models::Experiment> {
    use crate::api::admin::experiments::audit_row;
    use crate::db::repositories::experiments::ExperimentRepository;

    let before = ExperimentRepository::get(&**db, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no experiment with id {id}"))?;
    if before.closed_at.is_some() {
        anyhow::bail!("experiment {id} is already closed");
    }
    let closed_at = chrono::Utc::now().to_rfc3339();
    if !ExperimentRepository::close(&**db, id, &closed_at).await? {
        // Lost a race with the lifecycle tick or another operator.
        anyhow::bail!("experiment {id} is already closed");
    }
    let after = ExperimentRepository::get(&**db, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no experiment with id {id}"))?;

    admin::audit_change(
        &**db,
        "experiment.close",
        &format!("experiment:{id}"),
        Some(serde_json::json!({ "status": before.status, "closed_at": before.closed_at })),
        audit_row(&after),
    )
    .await;
    Ok(after)
}

/// `experiment results --id`: the endpoint's document, built from the CLI's
/// own sources — the main database, the dedicated prompt database when one
/// is configured, and the configured pricing. Never constructs an `AppState`.
async fn experiment_results(
    db: &Arc<dyn crate::api::app::DatabaseProvider>,
    settings: &crate::config::schema::Settings,
    id: i64,
    page: crate::api::admin::experiments::RunPage,
) -> anyhow::Result<crate::api::admin::experiments::ExperimentResults> {
    use crate::api::admin::experiments::{build_results, ExperimentSources};
    let prompt_db = open_prompt_db(settings, db).await?;
    let sources = ExperimentSources {
        db: db.clone(),
        prompt_db,
        cost_calc: Arc::new(crate::router::cost::CostCalculator::new_with_config(&settings.pricing)),
    };
    Ok(build_results(&sources, id, page).await?)
}

/// `expires_at` as an operator reads it: `never` for 0, else RFC3339.
fn render_expires_at(expires_at: i64) -> String {
    if expires_at == 0 {
        return "never".to_string();
    }
    chrono::DateTime::from_timestamp(expires_at, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| expires_at.to_string())
}

/// Whether content is retained and for how long after close.
fn render_retention(e: &crate::db::models::Experiment) -> String {
    match (e.retain_content, e.content_retention_days) {
        (false, _) => "no".to_string(),
        (true, 0) => "yes (never)".to_string(),
        (true, days) => format!("yes ({days}d)"),
    }
}

const EXPERIMENT_LIST_HEADERS: &[&str] = &[
    "ID", "Name", "Status", "Variants", "Expires", "Retain (Window)", "Created", "Closed",
];

fn experiment_list_row(e: &crate::db::models::Experiment) -> Vec<String> {
    vec![
        e.id.to_string(),
        e.name.clone(),
        e.status.as_str().to_string(),
        e.variants.keys().cloned().collect::<Vec<_>>().join(","),
        render_expires_at(e.expires_at),
        render_retention(e),
        e.created_at.clone(),
        e.closed_at.clone().unwrap_or_else(|| "-".to_string()),
    ]
}

/// Render a results document: the full JSON (identical to the endpoint's),
/// or the per-variant summary and the page of runs as two tables (with a
/// heading and notes) or two CSV blocks separated by a blank line.
fn write_experiment_results(
    r: &crate::api::admin::experiments::ExperimentResults,
    format: OutputFormat,
    out: &mut impl std::io::Write,
) -> std::io::Result<()> {
    use crate::report::formatter::write_rows;

    if matches!(format, OutputFormat::Json) {
        return writeln!(out, "{}", serde_json::to_string_pretty(r)?);
    }
    let table = matches!(format, OutputFormat::Table);

    let dash = || "-".to_string();
    let usd = |v: f64| format!("{:.4}", v);
    let one = |v: f64| format!("{:.1}", v);
    let pct = |v: Option<f64>| v.map(|v| format!("{:.1}%", v * 100.0)).unwrap_or_else(dash);
    let yes_no = |b: bool| if b { "yes" } else { "no" }.to_string();

    let e = &r.experiment;
    if table {
        writeln!(
            out,
            "Experiment {} '{}' ({}; expires {}; retain content {}; computed at {})",
            e.id,
            e.name,
            e.status.as_str(),
            render_expires_at(e.expires_at),
            render_retention(e),
            r.computed_at,
        )?;
        writeln!(out, "Variants:")?;
    }

    let mut variant_rows: Vec<Vec<String>> = r
        .variants
        .iter()
        .map(|v| {
            vec![
                v.label.clone(),
                v.runs.to_string(),
                v.mixed_runs.to_string(),
                v.requests.to_string(),
                v.unbound_requests.to_string(),
                v.turns.to_string(),
                usd(v.cost_usd),
                usd(v.saved_usd),
                v.tokens.prompt.to_string(),
                v.tokens.completion.to_string(),
                v.estimated_rows.to_string(),
                v.failures.to_string(),
                v.latency.as_ref().and_then(|l| l.mean_ms).map(one).unwrap_or_else(dash),
                v.latency_samples.to_string(),
                pct(v.outcomes.success_rate),
                yes_no(v.unpriced),
            ]
        })
        .collect();
    let t = &r.totals;
    variant_rows.push(vec![
        "TOTAL".to_string(),
        t.runs.to_string(),
        t.mixed_runs.to_string(),
        t.requests.to_string(),
        t.unbound_requests.to_string(),
        t.turns.to_string(),
        usd(t.cost_usd),
        usd(t.saved_usd),
        t.tokens.prompt.to_string(),
        t.tokens.completion.to_string(),
        t.estimated_rows.to_string(),
        t.failures.to_string(),
        dash(),
        t.latency_samples.to_string(),
        pct(t.outcomes.success_rate),
        yes_no(r.variants.iter().any(|v| v.unpriced)),
    ]);
    write_rows(
        &variant_rows,
        &[
            "Variant", "Runs", "Mixed", "Requests", "Unbound", "Turns", "Cost (USD)",
            "Saved (USD)", "Tokens In", "Tokens Out", "Estimated", "Failures",
            "Latency (ms)", "Samples", "Success Rate", "Unpriced",
        ],
        |row| row.clone(),
        format.clone(),
        out,
    )?;

    let runs = &r.runs;
    if table {
        let first = if runs.items.is_empty() { 0 } else { runs.offset + 1 };
        let last = runs.offset + runs.items.len() as i64;
        writeln!(out, "Runs {first}-{last} of {}:", runs.total)?;
    } else {
        writeln!(out)?;
    }
    let run_rows: Vec<Vec<String>> = runs
        .items
        .iter()
        .map(|run| {
            vec![
                run.user_id.to_string(),
                run.correlation_id.clone(),
                run.variant.clone(),
                yes_no(run.mixed),
                run.turns.to_string(),
                run.unbound_requests.to_string(),
                usd(run.cost_usd),
                run.tokens.prompt.to_string(),
                run.tokens.completion.to_string(),
                run.failures.to_string(),
                run.latency.map(|l| one(l.mean_ms)).unwrap_or_else(dash),
                run.latency_samples.to_string(),
                one(run.span_secs),
                run.first_at.clone(),
                run.last_at.clone(),
                run.outcome.as_ref().map(|o| o.outcome.clone()).unwrap_or_else(dash),
            ]
        })
        .collect();
    write_rows(
        &run_rows,
        &[
            "User", "Correlation ID", "Variant", "Mixed", "Turns", "Unbound", "Cost (USD)",
            "Tokens In", "Tokens Out", "Failures", "Latency (ms)", "Samples", "Span (s)",
            "First", "Last", "Outcome",
        ],
        |row| row.clone(),
        format,
        out,
    )?;

    if table {
        for v in &r.variants {
            if v.unpriced {
                writeln!(
                    out,
                    "Unpriced: variant {} includes {} — its cost figures are incomplete.",
                    v.label,
                    v.unpriced_models.join(", ")
                )?;
            }
        }
        if t.mixed_runs > 0 {
            writeln!(
                out,
                "Note: {} run(s) were seen under more than one variant; each is attributed to the variant of its earliest request.",
                t.mixed_runs
            )?;
        }
        if t.latency_samples == 0 && t.requests > 0 {
            writeln!(out, "Note: no prompt rows carry a latency measurement, so latency is not reported.")?;
        }
        if let Some(bytes) = r.retained_content_bytes {
            writeln!(out, "Retained content: {bytes} bytes.")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod experiment_cli_tests {
    use super::*;
    use crate::api::admin::experiments::RunPage;
    use crate::cli::commands::{ExperimentAddArgs, ExperimentArgs, ExperimentCommands};
    use crate::config::schema::{LbPoolEntry, LoadBalancerConfig, ProviderConfig, Settings};
    use crate::db::models::NewUser;
    use crate::db::repositories::audit::AuditRepository;
    use crate::db::repositories::users::UserRepository;
    use clap::Parser;

    type Db = Arc<dyn crate::api::app::DatabaseProvider>;

    /// The API test harness's world: a config alias to a priced model, one
    /// to an unpriced model, a pool, one configured provider and one user.
    fn settings() -> Settings {
        let mut s = Settings::default();
        s.routing
            .model_aliases
            .insert("fast".to_string(), "openai/gpt-4o-mini".to_string());
        s.routing
            .model_aliases
            .insert("mystery".to_string(), "openai/gpt-unpriced".to_string());
        s.providers.insert("openai".to_string(), ProviderConfig::default());
        s.routing.load_balancer.insert(
            "pool".to_string(),
            LoadBalancerConfig {
                strategy: Default::default(),
                pool: vec![LbPoolEntry {
                    provider: "openai".to_string(),
                    model: "gpt-4o".to_string(),
                    weight: 1,
                }],
            },
        );
        s
    }

    async fn db() -> Db {
        let db = crate::db::sqlite::SqliteDb::connect(":memory:").await.unwrap();
        crate::db::migrations::run_migrations(&db.pool).await.unwrap();
        UserRepository::create(&db, NewUser { name: "alice".to_string(), email: None })
            .await
            .unwrap();
        Arc::new(db)
    }

    /// Parse `modelrouter experiment add <flags>` exactly as `main` would.
    fn parse_add(flags: &[&str]) -> Result<ExperimentAddArgs, clap::Error> {
        let mut argv = vec!["modelrouter", "experiment", "add"];
        argv.extend_from_slice(flags);
        let cli = Cli::try_parse_from(argv)?;
        match cli.command {
            Commands::Experiment(ExperimentArgs { command: ExperimentCommands::Add(args) }) => {
                Ok(args)
            }
            _ => panic!("parsed something other than `experiment add`"),
        }
    }

    /// Two variants — an empty control and a candidate mapping `fast` —
    /// plus whatever the test adds.
    fn good_flags<'a>(name: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
        let mut flags = vec![
            "--name", name,
            "--variant", "control=",
            "--variant", "candidate=fast:openai/gpt-4o",
        ];
        flags.extend_from_slice(extra);
        flags
    }

    async fn add(db: &Db, flags: &[&str]) -> anyhow::Result<crate::db::models::Experiment> {
        let args = parse_add(flags).unwrap();
        experiment_add(db, &settings(), &args).await
    }

    #[tokio::test]
    async fn add_then_list_shows_the_row_and_results_json_is_the_document() {
        let db = db().await;
        let row = add(
            &db,
            &good_flags("exp", &["--expires-at", "never", "--content-retention-days", "0"]),
        )
        .await
        .unwrap();
        assert_eq!(row.name, "exp");
        // The candidate's target was resolved and pinned at creation.
        let pinned = &row.variants["candidate"]["fast"];
        assert_eq!(pinned.target, "openai/gpt-4o");
        assert_eq!((pinned.provider.as_str(), pinned.model.as_str()), ("openai", "gpt-4o"));
        assert!(row.variants["control"].is_empty());

        let listed = experiment_list(&db, "active").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, row.id);
        let cells = experiment_list_row(&listed[0]);
        assert_eq!(cells[0], row.id.to_string());
        assert_eq!(cells[1], "exp");
        assert_eq!(cells[2], "active");
        assert_eq!(cells[3], "candidate,control");
        assert_eq!(cells[4], "never");
        assert_eq!(cells[5], "no");
        assert_eq!(cells[7], "-");
        assert!(experiment_list(&db, "closed").await.unwrap().is_empty());
        assert_eq!(experiment_list(&db, "all").await.unwrap().len(), 1);

        let results = experiment_results(&db, &settings(), row.id, RunPage::default())
            .await
            .unwrap();
        let mut out = Vec::new();
        write_experiment_results(&results, OutputFormat::Json, &mut out).unwrap();
        let printed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(printed, serde_json::to_value(&results).unwrap());
        assert_eq!(printed["experiment"]["id"], row.id);
        assert_eq!(printed["experiment"]["name"], "exp");
        assert_eq!(printed["variants"].as_array().unwrap().len(), 2);
        assert_eq!(printed["runs"]["total"], 0);
        assert_eq!(printed["runs"]["limit"], 200);
        assert!(printed.get("retained_content_bytes").is_none());

        // Table and CSV carry the variant summary and the (empty) run page.
        let mut out = Vec::new();
        write_experiment_results(&results, OutputFormat::Table, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(&format!("Experiment {} 'exp' (active; expires never", row.id)), "{text}");
        assert!(text.contains("candidate"), "{text}");
        assert!(text.contains("TOTAL"), "{text}");
        assert!(text.contains("Runs 0-0 of 0:"), "{text}");
        let mut out = Vec::new();
        write_experiment_results(&results, OutputFormat::Csv, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("Variant,Runs,Mixed,Requests"), "{text}");
        assert!(text.contains("\n\nUser,Correlation ID,Variant"), "{text}");
    }

    #[tokio::test]
    async fn expires_at_never_stores_zero_and_rfc3339_stores_the_timestamp() {
        let db = db().await;
        let never = add(
            &db,
            &good_flags("never", &["--expires-at", "never", "--content-retention-days", "0"]),
        )
        .await
        .unwrap();
        assert_eq!(never.expires_at, 0);

        let dated = add(
            &db,
            &good_flags(
                "dated",
                &["--expires-at", "2999-01-01T00:00:00Z", "--content-retention-days", "7"],
            ),
        )
        .await
        .unwrap();
        assert_eq!(dated.expires_at, 32_472_144_000);
        assert_eq!(render_expires_at(dated.expires_at), "2999-01-01T00:00:00Z");
        assert_eq!(dated.content_retention_days, 7);

        // Not RFC3339 and in the past: refused in the API's words.
        let err = add(
            &db,
            &good_flags("bad", &["--expires-at", "tomorrow", "--content-retention-days", "0"]),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "expires_at must be an RFC3339 timestamp or 0 (never)");
        let err = add(
            &db,
            &good_flags(
                "past",
                &["--expires-at", "2000-01-01T00:00:00Z", "--content-retention-days", "0"],
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "expires_at must be in the future");
    }

    #[tokio::test]
    async fn retain_content_with_never_is_rejected_with_the_api_message() {
        let db = db().await;
        let err = add(
            &db,
            &good_flags(
                "retain",
                &["--retain-content", "--expires-at", "never", "--content-retention-days", "30"],
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "retain_content: true requires expires_at to be set; an experiment that never \
             expires cannot retain content"
        );
        assert!(experiment_list(&db, "all").await.unwrap().is_empty());

        // With a finite expiry the same flags are accepted and rendered.
        let row = add(
            &db,
            &good_flags(
                "retain",
                &[
                    "--retain-content",
                    "--expires-at",
                    "2999-01-01T00:00:00Z",
                    "--content-retention-days",
                    "30",
                ],
            ),
        )
        .await
        .unwrap();
        assert!(row.retain_content);
        assert_eq!(render_retention(&row), "yes (30d)");
    }

    #[tokio::test]
    async fn add_and_close_leave_cli_audit_rows_with_rendered_expiry_and_retention() {
        let db = db().await;
        let row = add(
            &db,
            &good_flags(
                "audited",
                &["--expires-at", "never", "--content-retention-days", "0", "--feed-learning"],
            ),
        )
        .await
        .unwrap();
        assert!(row.feed_learning);

        let entries = AuditRepository::list(&*db, 10, 0).await.unwrap();
        let created = entries
            .iter()
            .find(|e| e.action == "experiment.create")
            .expect("experiment.create audit row");
        assert_eq!(created.actor_name, "cli");
        assert_eq!(created.actor_id, None);
        assert_eq!(created.target.as_deref(), Some(format!("experiment:{}", row.id).as_str()));
        let after: serde_json::Value =
            serde_json::from_str(created.after_json.as_deref().unwrap()).unwrap();
        assert_eq!(after["expires_at"], "never");
        assert_eq!(after["content_retention_days"], "never");
        assert_eq!(after["name"], "audited");
        assert_eq!(after["feed_learning"], true);

        let closed = experiment_close(&db, row.id).await.unwrap();
        assert_eq!(closed.status, crate::db::models::ExperimentStatus::Closed);
        assert!(closed.closed_at.is_some());
        let entries = AuditRepository::list(&*db, 10, 0).await.unwrap();
        let close = entries
            .iter()
            .find(|e| e.action == "experiment.close")
            .expect("experiment.close audit row");
        assert_eq!(close.actor_name, "cli");
        assert_eq!(close.target.as_deref(), Some(format!("experiment:{}", row.id).as_str()));
        let before: serde_json::Value =
            serde_json::from_str(close.before_json.as_deref().unwrap()).unwrap();
        assert_eq!(before["status"], "active");
        assert_eq!(before["closed_at"], serde_json::Value::Null);
        let after: serde_json::Value =
            serde_json::from_str(close.after_json.as_deref().unwrap()).unwrap();
        assert_eq!(after["status"], "closed");
        assert_eq!(after["closed_at"], closed.closed_at.clone().unwrap());
        assert_eq!(after["expires_at"], "never");
        assert_eq!(after["content_retention_days"], "never");

        // The API's close semantics: a second close and an unknown id are errors.
        let err = experiment_close(&db, row.id).await.unwrap_err();
        assert_eq!(err.to_string(), format!("experiment {} is already closed", row.id));
        let err = experiment_close(&db, 999).await.unwrap_err();
        assert_eq!(err.to_string(), "no experiment with id 999");
        assert_eq!(experiment_list(&db, "closed").await.unwrap().len(), 1);
        assert!(experiment_list(&db, "active").await.unwrap().is_empty());
        assert_eq!(experiment_list_row(&closed)[2], "closed");
        assert_eq!(experiment_list_row(&closed)[7], closed.closed_at.clone().unwrap());
    }

    #[test]
    fn missing_expiry_or_retention_fails_at_clap_naming_the_flag() {
        let err = parse_add(&good_flags("x", &["--content-retention-days", "0"])).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        assert!(err.to_string().contains("--expires-at"), "{err}");

        let err = parse_add(&good_flags("x", &["--expires-at", "never"])).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        assert!(err.to_string().contains("--content-retention-days"), "{err}");

        // No default is ever substituted: both present parses, both absent
        // names both.
        let err = parse_add(&good_flags("x", &[])).unwrap_err();
        assert!(err.to_string().contains("--expires-at"), "{err}");
        assert!(err.to_string().contains("--content-retention-days"), "{err}");
        let args =
            parse_add(&good_flags("x", &["--expires-at", "never", "--content-retention-days", "0"]))
                .unwrap();
        assert_eq!(args.expires_at, "never");
        assert_eq!(args.content_retention_days, 0);
        assert!(!args.retain_content);

        // A variant flag is required too, and retention must be an integer.
        let err = parse_add(&["--name", "x", "--expires-at", "never", "--content-retention-days", "0"])
            .unwrap_err();
        assert!(err.to_string().contains("--variant"), "{err}");
        let err = parse_add(&good_flags("x", &["--expires-at", "never", "--content-retention-days", "many"]))
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[tokio::test]
    async fn gate_refuses_unpriced_pools_substitutions_and_unknown_providers_in_the_api_words() {
        let db = db().await;
        let flags = |variant: &'static str| {
            vec![
                "--name", "gated",
                "--variant", "control=",
                "--variant", variant,
                "--expires-at", "never",
                "--content-retention-days", "0",
            ]
        };

        // The literal the API produces for the same body.
        let err = add(&db, &flags("candidate=fast:openai/gpt-unpriced")).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "variants: variant 'candidate' key 'fast' target 'openai/gpt-unpriced' resolves to \
             'openai/gpt-unpriced', which has no pricing entry"
        );
        // Through a config alias the pinned pair is named.
        let err = add(&db, &flags("candidate=fast:mystery")).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "variants: variant 'candidate' key 'fast' target 'mystery' resolves to \
             'openai/gpt-unpriced', which has no pricing entry"
        );
        let err = add(&db, &flags("candidate=fast:pool")).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "variants: variant 'candidate' key 'fast' target 'pool' is a load balancer pool; \
             an experiment must pin one provider/model"
        );
        let err = add(&db, &flags("candidate=fast:no-such-model")).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "variants: variant 'candidate' key 'fast' target 'no-such-model' is not an alias or \
             provider/model and would be substituted with the default model"
        );
        // Priced, but `[providers.anthropic]` is not in this config.
        let err = add(&db, &flags("candidate=fast:anthropic/claude-haiku-4-5")).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "variants: variant 'candidate' key 'fast' target 'anthropic/claude-haiku-4-5' \
             resolves to unconfigured provider 'anthropic'"
        );
        // Nothing was stored by the refused commands.
        assert!(experiment_list(&db, "all").await.unwrap().is_empty());

        // A runtime alias set through `alias set` resolves like a config one.
        db.upsert_alias(crate::db::models::NewModelAlias {
            alias: "runtime".to_string(),
            target: "fast".to_string(),
            created_by: Some("cli".to_string()),
        })
        .await
        .unwrap();
        let row = add(&db, &flags("candidate=fast:runtime")).await.unwrap();
        let pinned = &row.variants["candidate"]["fast"];
        assert_eq!(pinned.target, "runtime");
        assert_eq!(pinned.model, "gpt-4o-mini");

        // The name is unique, in the API's words.
        let err = add(&db, &flags("candidate=fast:openai/gpt-4o")).await.unwrap_err();
        assert_eq!(err.to_string(), "name 'gated' is already taken");
    }

    #[tokio::test]
    async fn cli_created_experiment_is_bindable_after_registry_reload() {
        use crate::router::experiments::{ExperimentRegistry, EXPERIMENT_HEADER};

        let db = db().await;
        let row = add(
            &db,
            &good_flags("bind", &["--expires-at", "never", "--content-retention-days", "0"]),
        )
        .await
        .unwrap();

        // What the server's 60-second tick does, without the wait.
        let registry = ExperimentRegistry::default();
        assert!(registry.is_empty());
        registry.load_from(&*db).await.unwrap();
        assert_eq!(registry.len(), 1);

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            EXPERIMENT_HEADER,
            format!("{}:candidate", row.id).parse().unwrap(),
        );
        let binding = registry
            .bind(&headers, &serde_json::json!({}), Some("run-1"), 1, chrono::Utc::now().timestamp())
            .unwrap()
            .expect("bound");
        assert_eq!(binding.experiment_id, row.id);
        assert_eq!(binding.variant, "candidate");
        assert_eq!(binding.overlay.get("fast").map(String::as_str), Some("openai/gpt-4o"));
        assert!(!binding.retain_content);

        // Closing on the CLI is honoured by the next reload.
        experiment_close(&db, row.id).await.unwrap();
        registry.load_from(&*db).await.unwrap();
        let err = registry
            .bind(&headers, &serde_json::json!({}), Some("run-1"), 1, chrono::Utc::now().timestamp())
            .unwrap_err();
        assert_eq!(err, crate::router::experiments::BindError::Closed(row.id));
    }

    #[tokio::test]
    async fn allow_user_resolves_names_and_names_an_unknown_one() {
        let db = db().await;
        let alice = UserRepository::find_by_name(&*db, "alice").await.unwrap().unwrap();
        let row = add(
            &db,
            &good_flags(
                "scoped",
                &[
                    "--expires-at", "never",
                    "--content-retention-days", "0",
                    "--allow-user", "alice",
                    "--allow-user", "alice",
                ],
            ),
        )
        .await
        .unwrap();
        assert_eq!(row.allowed_user_ids, vec![alice.id]);

        let err = add(
            &db,
            &good_flags(
                "scoped2",
                &["--expires-at", "never", "--content-retention-days", "0", "--allow-user", "bob"],
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "--allow-user: no user named 'bob'");
    }

    #[test]
    fn variant_flags_become_the_api_body() {
        let v = parse_variant_flags(&[
            "control=".to_string(),
            "candidate=fast:openai/gpt-4o, deep : anthropic/claude-opus-4-6".to_string(),
        ])
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "control": {},
                "candidate": { "fast": "openai/gpt-4o", "deep": "anthropic/claude-opus-4-6" }
            })
        );

        let err = parse_variant_flags(&["control".to_string()]).unwrap_err();
        assert!(err.to_string().contains("LABEL=KEY:TARGET"), "{err}");
        let err = parse_variant_flags(&["a=fast".to_string()]).unwrap_err();
        assert!(err.to_string().contains("must be KEY:TARGET"), "{err}");
        let err = parse_variant_flags(&["a=fast:x,fast:y".to_string()]).unwrap_err();
        assert!(err.to_string().contains("key 'fast' appears more than once"), "{err}");
        let err = parse_variant_flags(&["a=".to_string(), "a=".to_string()]).unwrap_err();
        assert_eq!(err.to_string(), "variants: label 'a' appears more than once");

        // A single variant reaches the shared validator and is refused there.
        let args = parse_add(&["--name", "one", "--variant", "only=", "--expires-at", "never", "--content-retention-days", "0"]).unwrap();
        let body = experiment_create_body(&args, parse_variant_flags(&args.variants).unwrap(), vec![]);
        assert_eq!(body["expires_at"], 0);
        let err = crate::api::admin::experiments::parse_create(&body, chrono::Utc::now()).unwrap_err();
        assert_eq!(err, "variants must have 2-16 entries, got 1");
    }

    #[tokio::test]
    async fn list_status_and_results_page_use_the_endpoint_words() {
        let db = db().await;
        let err = experiment_list(&db, "bogus").await.unwrap_err();
        assert_eq!(err.to_string(), "status must be active, closed or all, got 'bogus'");
        let err = experiment_results(&db, &settings(), 42, RunPage::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "no experiment with id 42");
        assert_eq!(
            RunPage::parse(Some("0"), None).unwrap_err(),
            "limit must be an integer between 1 and 1000"
        );
    }
}
