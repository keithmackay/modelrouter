//! `serve --check-config`: prove a binary and its config can start, without
//! serving. A platform runs this from a new image before it stops the old
//! one. Every check runs even after one fails, so the report names every
//! problem in one pass; the exit code is non-zero if any failed.
//!
//! Nothing here applies a migration, syncs hook permissions, bootstraps an
//! admin, or binds a socket. The database checked is the one `serve` would
//! open: PostgreSQL when `database.postgres_url` is set, else the SQLite
//! file, which opening creates when it is absent, exactly as `serve` would.

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

    report.check("database", main_database_status(&settings.database).await);
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

/// The store `serve` would open, checked without applying anything.
async fn main_database_status(
    config: &crate::config::schema::DatabaseConfig,
) -> anyhow::Result<String> {
    match crate::db::postgres_url(config) {
        Some(url) => postgres_database_status(url).await,
        None => database_status(&config.path).await,
    }
}

#[cfg(feature = "postgres")]
async fn postgres_database_status(url: &str) -> anyhow::Result<String> {
    let db = crate::db::postgres::PostgresDb::connect(url).await?;
    sqlx::query("SELECT 1").execute(&db.pool).await?;
    let status = crate::db::migrations::postgres_migration_status(&db.pool).await?;
    Ok(format!(
        "{}, {} migrations applied, {} pending",
        describe_postgres_url(url),
        status.applied,
        status.pending.len()
    ))
}

#[cfg(not(feature = "postgres"))]
async fn postgres_database_status(_url: &str) -> anyhow::Result<String> {
    anyhow::bail!(crate::db::POSTGRES_FEATURE_MISSING)
}

/// `postgres host:port/database` from a connection URL, without the user,
/// password or query string, so the report never prints a credential.
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
fn describe_postgres_url(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, database) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if database.is_empty() {
        format!("postgres {host}")
    } else {
        format!("postgres {host}/{database}")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_postgres_url_is_described_without_its_credentials() {
        assert_eq!(
            describe_postgres_url("postgres://mr:s3cret@db.internal:5432/modelrouter?sslmode=require"),
            "postgres db.internal:5432/modelrouter"
        );
        assert_eq!(describe_postgres_url("postgresql://db/app"), "postgres db/app");
        assert_eq!(describe_postgres_url("postgres://u:p@host"), "postgres host");
        // An @ inside the password does not leak the rest of it.
        assert_eq!(describe_postgres_url("postgres://u:p@ss@host/db"), "postgres host/db");
        assert!(!describe_postgres_url("postgres://u:hunter2@h/d").contains("hunter2"));
    }

    #[tokio::test]
    async fn a_blank_postgres_url_checks_the_sqlite_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.db");
        let config = crate::config::schema::DatabaseConfig {
            path: path.to_string_lossy().into_owned(),
            postgres_url: Some("  ".into()),
        };
        let detail = main_database_status(&config).await.unwrap();
        assert!(detail.starts_with(&*path.to_string_lossy()), "{detail}");
    }

    #[cfg(not(feature = "postgres"))]
    #[tokio::test]
    async fn a_postgres_url_without_the_feature_fails_with_the_serve_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never.db");
        let config = crate::config::schema::DatabaseConfig {
            path: path.to_string_lossy().into_owned(),
            postgres_url: Some("postgres://u:p@localhost/db".into()),
        };
        let err = main_database_status(&config).await.unwrap_err();
        assert_eq!(err.to_string(), crate::db::POSTGRES_FEATURE_MISSING);
        assert!(!path.exists(), "the SQLite file must not be touched");
    }

    /// Live check against `MODELROUTER_TEST_POSTGRES_URL`; skipped when unset.
    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn a_live_postgres_store_is_checked_without_migrating() {
        let Ok(url) = std::env::var("MODELROUTER_TEST_POSTGRES_URL") else {
            eprintln!("MODELROUTER_TEST_POSTGRES_URL unset; skipping");
            return;
        };
        let config = crate::config::schema::DatabaseConfig {
            path: "/nonexistent/never.db".into(),
            postgres_url: Some(url.clone()),
        };
        let detail = main_database_status(&config).await.unwrap();
        assert!(detail.starts_with("postgres "), "{detail}");
        assert!(detail.contains("migrations applied"), "{detail}");
        assert!(!detail.contains('@'), "credentials must not be printed: {detail}");
    }
}
