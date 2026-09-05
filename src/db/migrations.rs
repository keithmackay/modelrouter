use sha2::{Digest, Sha256};

pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Checksums a migration file legitimately carried in the past.
///
/// sqlx checksums the whole migration file, comments included, and refuses to
/// run when a recorded checksum no longer matches the file on disk. That guard
/// is right nearly always: an edited migration usually means the schema a
/// database actually has is not the schema the file now describes, and running
/// on regardless would corrupt it quietly.
///
/// It misfires on an edit that changed no SQL. One such edit shipped — a
/// comment inside 024 named a downstream caller and was replaced with neutral
/// wording — and every database migrated before it now refuses to start with
/// "migration 24 was previously applied but has been modified". The schema is
/// identical; only the comment bytes moved.
///
/// Entries here are deliberately narrow: an exact `(version, previous checksum)`
/// pair, healed only to the checksum the current file has. Any mismatch that is
/// not listed here is still a hard error, so this cannot be used to wave through
/// a migration whose SQL really did change. Add an entry only for an edit that
/// provably leaves the SQL untouched.
const INERT_CHECKSUM_EDITS: &[(i64, &str)] = &[
    // 024_request_failures.sql: a comment naming a specific downstream caller
    // was replaced with neutral wording. No statement changed.
    (
        24,
        "F161CC083D53D73F75098CA1AC93DED1CE49ACB9971F533B3574F4FD3DBFE7F867B178501986BCD0C75C2D09286A2E01",
    ),
];

pub async fn run_migrations(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    heal_inert_checksum_edits(pool).await?;
    sqlx::migrate!("./migrations").run(pool).await?;
    warn_if_dev_key_active(pool).await?;
    Ok(())
}

/// Re-stamp migrations whose file was edited without changing its SQL, so a
/// database migrated before such an edit still starts.
///
/// Runs before the migrator. A fresh database has no history to heal and is
/// left alone; a checksum that matches neither the current file nor a listed
/// previous value is left alone too, so the migrator still refuses it.
async fn heal_inert_checksum_edits(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let recorded: Option<(String,)> =
        sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'")
            .fetch_optional(pool)
            .await?;
    if recorded.is_none() {
        return Ok(());
    }

    let migrator = sqlx::migrate!("./migrations");
    for (version, previous_hex) in INERT_CHECKSUM_EDITS {
        let Some(migration) = migrator.iter().find(|m| m.version == *version) else {
            continue;
        };
        let previous = hex::decode(previous_hex)
            .map_err(|e| anyhow::anyhow!("migration {version} has an unreadable previous checksum: {e}"))?;
        let current: &[u8] = migration.checksum.as_ref();
        if previous == current {
            continue;
        }

        let stored: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT checksum FROM _sqlx_migrations WHERE version = ?")
                .bind(version)
                .fetch_optional(pool)
                .await?;
        let Some((stored,)) = stored else { continue };
        if stored != previous {
            continue;
        }

        sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
            .bind(current)
            .bind(version)
            .execute(pool)
            .await?;
        tracing::warn!(
            version,
            "migration file was edited without changing its SQL; re-stamped the \
             recorded checksum so this database can continue"
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    /// The checksum 024 carried before the comment edit, as recorded by any
    /// database migrated at that time.
    const PREVIOUS_024: &str = "F161CC083D53D73F75098CA1AC93DED1CE49ACB9971F533B3574F4FD3DBFE7F867B178501986BCD0C75C2D09286A2E01";

    async fn migrated_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        run_migrations(&db.pool).await.unwrap();
        db
    }

    async fn set_checksum(db: &SqliteDb, version: i64, hex_value: &str) {
        sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
            .bind(hex::decode(hex_value).unwrap())
            .bind(version)
            .execute(&db.pool)
            .await
            .unwrap();
    }

    async fn checksum(db: &SqliteDb, version: i64) -> Vec<u8> {
        let (c,): (Vec<u8>,) =
            sqlx::query_as("SELECT checksum FROM _sqlx_migrations WHERE version = ?")
                .bind(version)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        c
    }

    /// The blocker: a database migrated before the comment edit must still
    /// start, and must be left recording the checksum the file now has.
    #[tokio::test]
    async fn a_database_stamped_before_an_inert_edit_still_migrates() {
        let db = migrated_db().await;
        set_checksum(&db, 24, PREVIOUS_024).await;

        run_migrations(&db.pool)
            .await
            .expect("a checksum that drifted for a comment-only edit must not block startup");

        let migrator = sqlx::migrate!("./migrations");
        let current = migrator.iter().find(|m| m.version == 24).unwrap();
        assert_eq!(
            checksum(&db, 24).await,
            current.checksum.as_ref(),
            "the healed row should record the current file's checksum"
        );
    }

    /// The guard the healing must not weaken: an unrecognised checksum is a
    /// migration whose SQL may genuinely differ, and still refuses to run.
    #[tokio::test]
    async fn an_unrecognised_checksum_still_refuses_to_migrate() {
        let db = migrated_db().await;
        let bogus = "00".repeat(48);
        set_checksum(&db, 24, &bogus).await;

        let err = run_migrations(&db.pool)
            .await
            .expect_err("an unknown checksum must remain a hard error");
        assert!(
            err.to_string().contains("modified"),
            "expected a checksum complaint, got: {err}"
        );
        assert_eq!(
            checksum(&db, 24).await,
            hex::decode(&bogus).unwrap(),
            "a refused migration must not have been re-stamped"
        );
    }

    /// Healing is scoped to the listed version: drift elsewhere is untouched.
    #[tokio::test]
    async fn an_unlisted_version_is_not_healed() {
        let db = migrated_db().await;
        let bogus = "11".repeat(48);
        set_checksum(&db, 23, &bogus).await;

        assert!(run_migrations(&db.pool).await.is_err(), "version 23 is not listed");
        assert_eq!(checksum(&db, 23).await, hex::decode(&bogus).unwrap());
    }

    /// A fresh database has no recorded history, so healing is a no-op rather
    /// than an error about a missing table.
    #[tokio::test]
    async fn a_fresh_database_migrates_cleanly() {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        heal_inert_checksum_edits(&db.pool)
            .await
            .expect("no migration table yet is not an error");
        run_migrations(&db.pool).await.unwrap();
    }

    /// Every listed pair must name a migration that exists and must actually
    /// differ from it, so a stale entry cannot sit here unnoticed.
    #[tokio::test]
    async fn listed_edits_refer_to_real_superseded_checksums() {
        let migrator = sqlx::migrate!("./migrations");
        for (version, previous_hex) in INERT_CHECKSUM_EDITS {
            let migration = migrator
                .iter()
                .find(|m| m.version == *version)
                .unwrap_or_else(|| panic!("listed migration {version} does not exist"));
            let previous = hex::decode(previous_hex)
                .unwrap_or_else(|e| panic!("migration {version} has unreadable hex: {e}"));
            assert_ne!(
                previous,
                migration.checksum.as_ref(),
                "migration {version} matches its listed previous checksum; the entry is stale"
            );
        }
    }
}
