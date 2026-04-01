use modelrouter::db::{migrations::run_migrations, sqlite::SqliteDb};

pub async fn in_memory_db() -> SqliteDb {
    let db = SqliteDb::connect(":memory:").await.unwrap();
    run_migrations(&db.pool).await.unwrap();
    db
}
