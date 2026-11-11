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

/// PostgreSQL migration files that once held a version number they no longer
/// own, as `(version, checksum the file had, file name)`.
///
/// Branches landed three files numbered 035 and two numbered 037 in
/// `migrations/postgres`. sqlx keys migrations by version, so no database
/// could migrate past 035 with all of them present: a fresh store applied the
/// first 035 and then failed inserting the second, and every later start
/// failed the checksum comparison. Each version now has one file (the one that
/// matches the SQLite migration of the same number), and the files that lost
/// their slot were replayed as later, fully guarded migrations:
///
/// - `035_backend_parity` and `035_schema_parity` became `042_schema_parity`
///   (plus `043` widening the column `035_backend_parity` created as INT4);
/// - `037_call_kind` became `041_call_kind`.
///
/// A store that recorded one of these files under its old version would fail
/// the checksum comparison against the file that owns that version now. Before
/// migrating, the recorded row is dropped, so the migrator runs the owning
/// file, and later the replay. Both are idempotent, so the schema converges
/// whichever file the store applied first. Only an exact `(version, checksum)`
/// pair from this list is dropped; any other mismatch still stops startup.
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
const RETIRED_POSTGRES_MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        35,
        "ED6A07BD6B140C2734E1333B6F9B2E0B7B3BFFAC6529792F943E8D1C7A28F803D52838600A6FD7D87801AB5D898CE13B",
        "035_backend_parity.sql",
    ),
    (
        35,
        "F2525B75B0E3E5F68E618A6D94184F8B7D7EBCCA0F7688698CD3DA4F64BF01EDBBCFD0FC699F0848D5CFE652574FA817",
        "035_schema_parity.sql",
    ),
    (
        37,
        "22DD1A6F72114E07FE7CAB4E749F55CF680E53920A66D92F58DB05E4405DF74521D5CE9E87DC1F6619DF2B1821CD02C5",
        "037_call_kind.sql",
    ),
];

/// Apply the Postgres schema (`migrations/postgres`), first releasing any
/// version a store recorded for a file that no longer owns it (see
/// [`RETIRED_POSTGRES_MIGRATIONS`]).
#[cfg(feature = "postgres")]
pub async fn run_postgres_migrations(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    release_retired_postgres_versions(pool).await?;
    sqlx::migrate!("./migrations/postgres").run(pool).await?;
    Ok(())
}

#[cfg(feature = "postgres")]
async fn release_retired_postgres_versions(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    // `to_regclass` resolves against the search path, the same way the
    // migrator finds its table, and is NULL on a fresh store.
    let (table,): (Option<String>,) =
        sqlx::query_as("SELECT to_regclass('_sqlx_migrations')::text")
            .fetch_one(pool)
            .await?;
    if table.is_none() {
        return Ok(());
    }

    for (version, checksum_hex, file) in RETIRED_POSTGRES_MIGRATIONS {
        let checksum = hex::decode(checksum_hex)
            .map_err(|e| anyhow::anyhow!("retired migration {file} has an unreadable checksum: {e}"))?;
        let released = sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1 AND checksum = $2")
            .bind(version)
            .bind(&checksum)
            .execute(pool)
            .await?
            .rows_affected();
        if released > 0 {
            tracing::warn!(
                version,
                file,
                "this database recorded a migration file that no longer owns its version; \
                 released the version so the current file and its replay can run"
            );
        }
    }
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

    fn sha384_hex(bytes: &[u8]) -> String {
        use sha2::Sha384;
        hex::encode_upper(Sha384::digest(bytes))
    }

    /// The retired checksums were computed as SHA-384 of the file bytes. That
    /// is only meaningful if it is exactly what sqlx records, so check the
    /// scheme against every PostgreSQL migration still on disk.
    #[test]
    fn sqlx_checksum_is_sha384_of_the_file() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/postgres");
        let migrator = sqlx::migrate!("./migrations/postgres");
        for migration in migrator.iter() {
            let prefix = format!("{:03}_", migration.version);
            let file = std::fs::read_dir(&root)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.file_name().unwrap().to_string_lossy().starts_with(&prefix))
                .unwrap_or_else(|| panic!("no file for postgres migration {}", migration.version));
            assert_eq!(
                sha384_hex(&std::fs::read(&file).unwrap()),
                hex::encode_upper(migration.checksum.as_ref()),
                "{}",
                file.display()
            );
        }
    }

    /// Each retired entry names a file that is gone, at a version another file
    /// now owns with a different checksum. Releasing the recorded row is then
    /// always followed by the migrator running the owning file.
    #[test]
    fn retired_postgres_migrations_are_gone_and_their_versions_are_owned() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/postgres");
        let migrator = sqlx::migrate!("./migrations/postgres");
        for (version, checksum_hex, file) in RETIRED_POSTGRES_MIGRATIONS {
            assert!(!root.join(file).exists(), "{file} is listed as retired but is still on disk");
            let owner = migrator
                .iter()
                .find(|m| m.version == *version)
                .unwrap_or_else(|| panic!("no migration owns version {version} now"));
            assert_ne!(
                hex::encode_upper(owner.checksum.as_ref()),
                *checksum_hex,
                "{file}: version {version} is still owned by the retired file"
            );
        }
    }

    /// 037_call_kind moved to 041 byte for byte, so a store that applied it as
    /// 037 already has everything 041 does; 041 re-runs as a guarded no-op.
    #[test]
    fn call_kind_moved_to_041_unchanged() {
        let migrator = sqlx::migrate!("./migrations/postgres");
        let moved = migrator.iter().find(|m| m.version == 41).expect("041 exists");
        let (_, retired, _) = RETIRED_POSTGRES_MIGRATIONS
            .iter()
            .find(|(v, _, f)| *v == 37 && *f == "037_call_kind.sql")
            .expect("037_call_kind is listed");
        assert_eq!(hex::encode_upper(moved.checksum.as_ref()), *retired);
    }

    /// The replays must be safe to run on a store that already applied the
    /// file they replace: every DDL statement in them is guarded.
    #[test]
    fn postgres_replays_are_guarded() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/postgres");
        for file in ["041_call_kind.sql", "042_schema_parity.sql"] {
            let sql = std::fs::read_to_string(root.join(file)).unwrap();
            for stmt in sql
                .lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n")
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                assert!(stmt.contains("IF NOT EXISTS"), "{file}: unguarded statement: {stmt}");
            }
        }
        let widen = std::fs::read_to_string(root.join("043_budget_max_concurrent_bigint.sql")).unwrap();
        assert!(widen.contains("data_type = 'integer'"), "043 must only widen an INT4 column");
    }
}
