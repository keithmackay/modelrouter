use anyhow::Result;
use crate::cli::commands::AdminCommands;
use crate::db::models::NewAuditLogEntry;
use crate::db::repositories::{
    admin_users::AdminUserRepository,
    audit::AuditRepository,
};
use crate::report::formatter::print_rows;

/// DB bootstrap shared by all admin commands.
/// NOTE: Always uses SQLite. The `--features postgres` build still uses SQLite
/// for CLI commands — Postgres is only wired at serve time via AppState.
async fn connect(config: Option<std::path::PathBuf>) -> Result<crate::db::sqlite::SqliteDb> {
    let settings = crate::config::load(config)?;
    let db = crate::db::sqlite::SqliteDb::connect(&settings.database.path).await?;
    crate::db::migrations::run_migrations(&db.pool).await?;
    Ok(db)
}

/// Write an audit row with actor = "cli". Logs a warning on failure but does not abort.
async fn audit(
    db: &impl AuditRepository,
    action: &str,
    target: &str,
    after_json: serde_json::Value,
) {
    // after_json is serde_json::Value; convert to String for the Option<String> field.
    if let Err(e) = AuditRepository::create(
        db,
        NewAuditLogEntry {
            actor_id: None,
            actor_name: "cli".to_string(),
            action: action.to_string(),
            target: Some(target.to_string()),
            before_json: None,
            after_json: Some(after_json.to_string()),
        },
    )
    .await
    {
        eprintln!("warning: failed to write audit log: {e}");
    }
}

pub async fn run(config: Option<std::path::PathBuf>, cmd: AdminCommands) -> Result<()> {
    match cmd {
        AdminCommands::Create { name, role } => {
            let db = connect(config).await?;

            // Check uniqueness before prompting for password
            if AdminUserRepository::find_by_name(&db, &name).await?.is_some() {
                anyhow::bail!("admin user '{}' already exists", name);
            }

            let password = rpassword::prompt_password("Password: ")?;
            let confirm = rpassword::prompt_password("Confirm password: ")?;
            if password != confirm {
                anyhow::bail!("passwords do not match");
            }

            let password_hash = bcrypt::hash(&password, bcrypt::DEFAULT_COST)
                .map_err(|e| anyhow::anyhow!("bcrypt error: {e}"))?;

            let admin = AdminUserRepository::create(
                &db,
                crate::db::models::NewAdminUser {
                    name: name.clone(),
                    password_hash,
                    role: role.to_string(),
                },
            )
            .await?;

            audit(
                &db,
                "admin.create",
                &format!("admin:{}", admin.id),
                serde_json::json!({ "name": admin.name, "role": admin.role }),
            )
            .await;

            println!("Created admin '{}' (id={}, role={}).", admin.name, admin.id, admin.role);
            println!("Store this password securely — it cannot be retrieved later.");
        }

        AdminCommands::List { format } => {
            let db = connect(config).await?;
            let admins = AdminUserRepository::list(&db).await?;

            // Project to a safe DTO — AdminUser derives Serialize which includes
            // password_hash. Using explicit row closure ensures the hash never
            // appears in table, CSV, or JSON output.
            #[derive(serde::Serialize)]
            struct AdminRow {
                id: i64,
                name: String,
                role: String,
                status: String,
                created_at: String,
            }
            let rows: Vec<AdminRow> = admins.into_iter().map(|a| AdminRow {
                id: a.id,
                name: a.name,
                role: a.role,
                status: if a.enabled { "enabled".into() } else { "disabled".into() },
                created_at: a.created_at,
            }).collect();

            print_rows(
                &rows,
                &["ID", "Name", "Role", "Status", "Created At"],
                |a| {
                    vec![
                        a.id.to_string(),
                        a.name.clone(),
                        a.role.clone(),
                        a.status.clone(),
                        a.created_at.clone(),
                    ]
                },
                format,
            );
        }

        AdminCommands::ResetPassword { name } => {
            let db = connect(config).await?;

            let admin = AdminUserRepository::find_by_name(&db, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("admin user '{}' not found", name))?;

            let password = rpassword::prompt_password("New password: ")?;
            let password_hash = bcrypt::hash(&password, bcrypt::DEFAULT_COST)
                .map_err(|e| anyhow::anyhow!("bcrypt error: {e}"))?;

            AdminUserRepository::update_password_hash(&db, admin.id, &password_hash).await?;

            audit(
                &db,
                "admin.reset_password",
                &format!("admin:{}", admin.id),
                serde_json::json!({ "name": admin.name }),
            )
            .await;

            println!("Password updated for admin '{}'.", admin.name);
        }

        AdminCommands::Enable { name } => {
            let db = connect(config).await?;

            let admin = AdminUserRepository::find_by_name(&db, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("admin user '{}' not found", name))?;

            AdminUserRepository::set_enabled(&db, admin.id, true).await?;

            audit(
                &db,
                "admin.enable",
                &format!("admin:{}", admin.id),
                serde_json::json!({ "name": admin.name, "enabled": true }),
            )
            .await;

            println!("Enabled admin '{}'.", admin.name);
        }

        AdminCommands::Disable { name } => {
            let db = connect(config).await?;

            let admin = AdminUserRepository::find_by_name(&db, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("admin user '{}' not found", name))?;

            AdminUserRepository::set_enabled(&db, admin.id, false).await?;

            audit(
                &db,
                "admin.disable",
                &format!("admin:{}", admin.id),
                serde_json::json!({ "name": admin.name, "enabled": false }),
            )
            .await;

            println!("Disabled admin '{}'.", admin.name);
        }
    }
    Ok(())
}
