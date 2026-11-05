//! `serve --check-config`: prove a binary and its config can start, without
//! serving. A platform runs this from a new image before it stops the old
//! one. Every check runs even after one fails, so the report names every
//! problem in one pass; the exit code is non-zero if any failed.
//!
//! Nothing here applies a migration, syncs hook permissions, bootstraps an
//! admin, or binds a socket. Opening the SQLite store creates the file when
//! it is absent, exactly as `serve` would.

use std::sync::Arc;

use crate::config::schema::Settings;

struct Report {
    problems: usize,
}

impl Report {
    fn ok(&self, what: &str, detail: impl AsRef<str>) {
        let detail = detail.as_ref();
        if detail.is_empty() {
            println!("{what}: ok");
        } else {
            println!("{what}: ok ({detail})");
        }
    }

    fn fail(&mut self, what: &str, error: impl std::fmt::Display) {
        self.problems += 1;
        println!("{what}: FAIL {error}");
    }

    fn check(&mut self, what: &str, result: anyhow::Result<String>) {
        match result {
            Ok(detail) => self.ok(what, detail),
            Err(e) => self.fail(what, e),
        }
    }
}

pub async fn run(settings: Arc<Settings>, config_path: Option<&str>) -> anyhow::Result<()> {
    let mut report = Report { problems: 0 };
    report.ok("config", config_path.unwrap_or("defaults and environment"));

    report.check(
        "provider features",
        crate::providers::validate_provider_features(&settings.providers).map(|_| String::new()),
    );
    report.check(
        "auth.jwt_secret",
        settings.auth.validate_secret().map(|_| String::new()),
    );
    if settings.oidc.enabled {
        report.check("oidc", settings.oidc.validate_role().map(|_| String::new()));
    } else {
        report.ok("oidc", "disabled");
    }

    report.check("database", database_status(&settings.database.path).await);
    match settings.storage.prompt_db_path.as_deref() {
        Some(path) => report.check("prompt database", store_answers(path).await),
        None => report.ok("prompt database", "main database"),
    }

    let registry = crate::providers::registry::ProviderRegistry::new_with_tier_timeouts(
        settings.providers.clone(),
        settings.tier_timeouts.clone(),
    );
    let mut names: Vec<&String> = settings
        .providers
        .iter()
        .filter(|(_, c)| c.generic_chat)
        .map(|(name, _)| name)
        .collect();
    names.sort();
    if names.is_empty() {
        report.ok("providers", "none configured for chat");
    }
    for name in names {
        report.check(
            &format!("provider {name}"),
            registry.get(name).map(|_| String::new()),
        );
    }

    if report.problems == 0 {
        println!("result: ok");
        Ok(())
    } else {
        println!("result: FAIL ({} problem(s))", report.problems);
        anyhow::bail!("check-config found {} problem(s)", report.problems)
    }
}

async fn database_status(path: &str) -> anyhow::Result<String> {
    let db = crate::db::sqlite::SqliteDb::connect(path).await?;
    sqlx::query("SELECT 1").execute(&db.pool).await?;
    let status = crate::db::migrations::migration_status(&db.pool).await?;
    Ok(format!(
        "{path}, {} migrations applied, {} pending",
        status.applied,
        status.pending.len()
    ))
}

async fn store_answers(path: &str) -> anyhow::Result<String> {
    let db = crate::db::sqlite::SqliteDb::connect(path).await?;
    sqlx::query("SELECT 1").execute(&db.pool).await?;
    Ok(path.to_string())
}
