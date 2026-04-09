use sha2::{Digest, Sha256};

pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

pub async fn run_migrations(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    warn_if_dev_key_active(pool).await?;
    Ok(())
}

pub async fn run_dev_seed(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    if std::env::var("MODELROUTER_DEV_SEED").as_deref() == Ok("true") {
        sqlx::query(include_str!("dev_seed.sql"))
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn warn_if_dev_key_active(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let dev_hash = hash_token("mr-dev-key");
    let row = sqlx::query("SELECT id FROM api_keys WHERE key_hash = ? AND enabled = 1")
        .bind(&dev_hash)
        .fetch_optional(pool)
        .await?;
    if row.is_some() {
        tracing::warn!(
            "SECURITY: default dev API key (mr-dev-key) is still active. \
             Rotate or disable before production use."
        );
    }
    Ok(())
}
