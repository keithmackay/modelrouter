//! `db export` and `db import`: move a whole store between backends as JSON.
//!
//! Schema-driven on purpose. The repository structs omit columns, so the
//! export reads the live catalogue (every user table, every column) and
//! writes rows as plain JSON values. The import reads the target catalogue
//! the same way, converts each value to the target column's type, inserts
//! inside one transaction in parent-first order, verifies per-table counts,
//! and only then commits. Key hashes travel as stored.

use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Column, Row, TypeInfo, ValueRef};

use crate::report::ReportPool;

pub const FORMAT_VERSION: u32 = 1;

/// sqlx's own bookkeeping; the target migrates itself before an import.
const SKIP_TABLES: &[&str] = &["_sqlx_migrations"];

/// Parent-first so foreign keys hold on a backend that checks them at once.
/// A table the list does not know is imported after these, alphabetically.
const IMPORT_ORDER: &[&str] = &[
    "users",
    "admin_users",
    "groups",
    "api_keys",
    "sessions",
    "group_memberships",
    "budget_rules",
    "hook_permissions",
    "audit_log",
    "mcp_servers",
    "prompts",
    "cost_ledger",
    "request_failures",
    "rate_limit_state",
    "run_outcomes",
    "models",
    "model_failovers",
    "webhook_callbacks",
    "model_aliases",
    "provider_states",
    "app_settings",
    "experiments",
    "learned_model_capabilities",
    "hook_metrics",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Dump {
    pub format_version: u32,
    pub source_backend: String,
    pub exported_at: String,
    pub tables: BTreeMap<String, TableDump>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TableDump {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// What an import did to one table, for the operator's eyes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableReport {
    pub table: String,
    pub inserted: usize,
    pub total_after: i64,
}

/// Bytes cannot ride in JSON as-is; a BLOB becomes `{"$bytes": "<base64>"}`.
fn bytes_to_json(bytes: &[u8]) -> Value {
    serde_json::json!({ "$bytes": base64::engine::general_purpose::STANDARD.encode(bytes) })
}

fn json_to_bytes(v: &Value) -> Option<Vec<u8>> {
    let encoded = v.get("$bytes")?.as_str()?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// ── Export ───────────────────────────────────────────────────────────────────

pub async fn export(pool: &ReportPool) -> anyhow::Result<Dump> {
    let tables = match pool {
        ReportPool::Sqlite(pool) => export_sqlite(pool).await?,
        #[cfg(feature = "postgres")]
        ReportPool::Postgres(pool) => export_postgres(pool).await?,
    };
    Ok(Dump {
        format_version: FORMAT_VERSION,
        source_backend: pool.backend().to_string(),
        exported_at: chrono::Utc::now().to_rfc3339(),
        tables,
    })
}

async fn export_sqlite(pool: &sqlx::SqlitePool) -> anyhow::Result<BTreeMap<String, TableDump>> {
    let names: Vec<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    let mut tables = BTreeMap::new();
    for (table,) in names {
        if SKIP_TABLES.contains(&table.as_str()) {
            continue;
        }
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT * FROM {}", quote_ident(&table))))
            .fetch_all(pool)
            .await?;
        let mut dump = TableDump::default();
        if let Some(first) = rows.first() {
            dump.columns = first
                .columns()
                .iter()
                .map(|c| c.name().to_string())
                .collect();
        } else {
            dump.columns = sqlite_columns(pool, &table)
                .await?
                .into_iter()
                .map(|(n, _)| n)
                .collect();
        }
        for row in &rows {
            let mut out = Vec::with_capacity(dump.columns.len());
            for i in 0..dump.columns.len() {
                out.push(sqlite_value(row, i)?);
            }
            dump.rows.push(out);
        }
        tables.insert(table, dump);
    }
    Ok(tables)
}

/// `(name, declared type)` for every column, in table order.
async fn sqlite_columns(
    pool: &sqlx::SqlitePool,
    table: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info({})", quote_ident(table))))
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("name"),
                r.get::<String, _>("type").to_uppercase(),
            )
        })
        .collect())
}

fn sqlite_value(row: &sqlx::sqlite::SqliteRow, i: usize) -> anyhow::Result<Value> {
    let raw = row.try_get_raw(i)?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    // The value's own storage class, not the column's declared affinity.
    Ok(match raw.type_info().name() {
        "INTEGER" | "INT64" | "BOOLEAN" => Value::from(row.try_get::<i64, _>(i)?),
        "REAL" | "NUMERIC" => Value::from(row.try_get::<f64, _>(i)?),
        "BLOB" => bytes_to_json(&row.try_get::<Vec<u8>, _>(i)?),
        _ => Value::from(row.try_get::<String, _>(i)?),
    })
}

#[cfg(feature = "postgres")]
async fn export_postgres(pool: &sqlx::PgPool) -> anyhow::Result<BTreeMap<String, TableDump>> {
    let names: Vec<(String,)> = sqlx::query_as(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = current_schema() AND table_type = 'BASE TABLE' ORDER BY table_name",
    )
    .fetch_all(pool)
    .await?;
    let mut tables = BTreeMap::new();
    for (table,) in names {
        if SKIP_TABLES.contains(&table.as_str()) {
            continue;
        }
        let columns = postgres_columns(pool, &table).await?;
        let select = columns
            .iter()
            .map(|(n, _)| quote_ident(n))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {select} FROM {}", quote_ident(&table))))
            .fetch_all(pool)
            .await?;
        let mut dump = TableDump {
            columns: columns.iter().map(|(n, _)| n.clone()).collect(),
            rows: Vec::with_capacity(rows.len()),
        };
        for row in &rows {
            let mut out = Vec::with_capacity(columns.len());
            for (i, (name, _)) in columns.iter().enumerate() {
                out.push(
                    postgres_value(row, i).map_err(|e| anyhow::anyhow!("{table}.{name}: {e}"))?,
                );
            }
            dump.rows.push(out);
        }
        tables.insert(table, dump);
    }
    Ok(tables)
}

/// `(name, data_type)` from the catalogue, in ordinal order.
#[cfg(feature = "postgres")]
async fn postgres_columns(
    pool: &sqlx::PgPool,
    table: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, data_type FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1 ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(feature = "postgres")]
fn postgres_value(row: &sqlx::postgres::PgRow, i: usize) -> anyhow::Result<Value> {
    let raw = row.try_get_raw(i)?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let type_name = raw.type_info().name().to_string();
    Ok(match type_name.as_str() {
        "INT8" => Value::from(row.try_get::<i64, _>(i)?),
        "INT4" => Value::from(row.try_get::<i32, _>(i)?),
        "INT2" => Value::from(row.try_get::<i16, _>(i)?),
        "FLOAT8" => Value::from(row.try_get::<f64, _>(i)?),
        "FLOAT4" => Value::from(row.try_get::<f32, _>(i)?),
        "BOOL" => Value::from(row.try_get::<bool, _>(i)?),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" => Value::from(row.try_get::<String, _>(i)?),
        "TIMESTAMPTZ" => Value::from(
            row.try_get::<chrono::DateTime<chrono::Utc>, _>(i)?
                .to_rfc3339(),
        ),
        "TIMESTAMP" => Value::from(
            row.try_get::<chrono::NaiveDateTime, _>(i)?
                .and_utc()
                .to_rfc3339(),
        ),
        "BYTEA" => bytes_to_json(&row.try_get::<Vec<u8>, _>(i)?),
        other => anyhow::bail!("column type {other} is not supported by db export"),
    })
}

// ── Import ───────────────────────────────────────────────────────────────────

/// Tables in the order they must be inserted.
fn ordered_tables(dump: &Dump) -> Vec<&str> {
    let mut ordered: Vec<&str> = IMPORT_ORDER
        .iter()
        .copied()
        .filter(|t| dump.tables.contains_key(*t))
        .collect();
    for name in dump.tables.keys() {
        if !IMPORT_ORDER.contains(&name.as_str()) {
            ordered.push(name);
        }
    }
    ordered
}

pub async fn import(pool: &ReportPool, dump: &Dump, merge: bool) -> anyhow::Result<Vec<TableReport>> {
    if dump.format_version != FORMAT_VERSION {
        anyhow::bail!(
            "dump format version {} is not supported by this binary (expected {FORMAT_VERSION})",
            dump.format_version
        );
    }
    match pool {
        ReportPool::Sqlite(pool) => import_sqlite(pool, dump, merge).await,
        #[cfg(feature = "postgres")]
        ReportPool::Postgres(pool) => import_postgres(pool, dump, merge).await,
    }
}

fn refuse_non_empty(counts: &[(String, i64)], merge: bool) -> anyhow::Result<()> {
    let occupied: Vec<String> = counts
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(t, n)| format!("{t} ({n})"))
        .collect();
    if !occupied.is_empty() && !merge {
        anyhow::bail!(
            "target already holds rows in {}; pass --merge to import into a non-empty store",
            occupied.join(", ")
        );
    }
    Ok(())
}

fn missing_columns(table: &str, source: &[String], target: &[String]) -> anyhow::Result<()> {
    let missing: Vec<&String> = source.iter().filter(|c| !target.contains(c)).collect();
    if missing.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "{table}: the target has no column(s) {}; migrate the target to a schema at least as new as the source",
        missing.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ")
    )
}

async fn import_sqlite(
    pool: &sqlx::SqlitePool,
    dump: &Dump,
    merge: bool,
) -> anyhow::Result<Vec<TableReport>> {
    let tables = ordered_tables(dump);
    let mut before = Vec::new();
    let mut target_columns: HashMap<&str, Vec<(String, String)>> = HashMap::new();
    for table in &tables {
        let cols = sqlite_columns(pool, table).await?;
        if cols.is_empty() {
            anyhow::bail!("{table}: the target has no such table; migrate it first");
        }
        let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {}", quote_ident(table))))
            .fetch_one(pool)
            .await?;
        before.push((table.to_string(), n));
        target_columns.insert(table, cols);
    }
    refuse_non_empty(&before, merge)?;

    let mut tx = pool.begin().await?;
    // Parent-first order already holds; this makes the import also survive a
    // dump whose tables reference each other in a cycle.
    sqlx::query("PRAGMA defer_foreign_keys = ON")
        .execute(&mut *tx)
        .await?;
    let mut reports = Vec::new();
    for (table, (_, had)) in tables.iter().zip(&before) {
        let src = &dump.tables[*table];
        let cols = &target_columns[table];
        let names: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
        missing_columns(table, &src.columns, &names)?;
        let sql = insert_sql(table, &src.columns, |_| "?".to_string());
        for row in &src.rows {
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.clone()));
            for (col, value) in src.columns.iter().zip(row) {
                let declared = cols
                    .iter()
                    .find(|(n, _)| n == col)
                    .map(|(_, t)| t.as_str())
                    .unwrap_or("");
                q = bind_sqlite(q, value, declared)
                    .map_err(|e| anyhow::anyhow!("{table}.{col}: {e}"))?;
            }
            q.execute(&mut *tx)
                .await
                .map_err(|e| anyhow::anyhow!("{table}: {e}"))?;
        }
        let (after,): (i64,) =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {}", quote_ident(table))))
                .fetch_one(&mut *tx)
                .await?;
        let expected = had + src.rows.len() as i64;
        if after != expected {
            anyhow::bail!("{table}: expected {expected} rows after import, found {after}; nothing was committed");
        }
        reports.push(TableReport {
            table: table.to_string(),
            inserted: src.rows.len(),
            total_after: after,
        });
    }
    tx.commit().await?;
    Ok(reports)
}

fn insert_sql(table: &str, columns: &[String], placeholder: impl Fn(usize) -> String) -> String {
    let cols = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let params = (1..=columns.len())
        .map(placeholder)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {} ({cols}) VALUES ({params})",
        quote_ident(table)
    )
}

type SqliteQuery<'q> = sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>;

fn bind_sqlite<'q>(
    q: SqliteQuery<'q>,
    value: &'q Value,
    declared: &str,
) -> anyhow::Result<SqliteQuery<'q>> {
    Ok(match value {
        Value::Null => q.bind(Option::<String>::None),
        Value::Bool(b) => q.bind(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if declared.contains("REAL")
                    || declared.contains("FLOA")
                    || declared.contains("DOUB")
                {
                    q.bind(i as f64)
                } else {
                    q.bind(i)
                }
            } else {
                q.bind(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => q.bind(s.as_str()),
        Value::Object(_) => match json_to_bytes(value) {
            Some(bytes) => q.bind(bytes),
            None => q.bind(value.to_string()),
        },
        Value::Array(_) => q.bind(value.to_string()),
    })
}

#[cfg(feature = "postgres")]
async fn import_postgres(
    pool: &sqlx::PgPool,
    dump: &Dump,
    merge: bool,
) -> anyhow::Result<Vec<TableReport>> {
    let tables = ordered_tables(dump);
    let mut before = Vec::new();
    let mut target_columns: HashMap<&str, Vec<(String, String)>> = HashMap::new();
    for table in &tables {
        let cols = postgres_columns(pool, table).await?;
        if cols.is_empty() {
            anyhow::bail!("{table}: the target has no such table; migrate it first");
        }
        let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {}", quote_ident(table))))
            .fetch_one(pool)
            .await?;
        before.push((table.to_string(), n));
        target_columns.insert(table, cols);
    }
    refuse_non_empty(&before, merge)?;

    let mut tx = pool.begin().await?;
    let mut reports = Vec::new();
    for (table, (_, had)) in tables.iter().zip(&before) {
        let src = &dump.tables[*table];
        let cols = &target_columns[table];
        let names: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
        missing_columns(table, &src.columns, &names)?;
        let sql = insert_sql(table, &src.columns, |i| format!("${i}"));
        for row in &src.rows {
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.clone()));
            for (col, value) in src.columns.iter().zip(row) {
                let data_type = cols
                    .iter()
                    .find(|(n, _)| n == col)
                    .map(|(_, t)| t.as_str())
                    .unwrap_or("text");
                q = bind_postgres(q, value, data_type)
                    .map_err(|e| anyhow::anyhow!("{table}.{col}: {e}"))?;
            }
            q.execute(&mut *tx)
                .await
                .map_err(|e| anyhow::anyhow!("{table}: {e}"))?;
        }
        if src.columns.iter().any(|c| c == "id") {
            reset_sequence(&mut tx, table).await?;
        }
        let (after,): (i64,) =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {}", quote_ident(table))))
                .fetch_one(&mut *tx)
                .await?;
        let expected = had + src.rows.len() as i64;
        if after != expected {
            anyhow::bail!("{table}: expected {expected} rows after import, found {after}; nothing was committed");
        }
        reports.push(TableReport {
            table: table.to_string(),
            inserted: src.rows.len(),
            total_after: after,
        });
    }
    tx.commit().await?;
    Ok(reports)
}

/// After inserting explicit ids, move the serial past the highest one so the
/// next insert does not collide.
#[cfg(feature = "postgres")]
async fn reset_sequence(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
) -> anyhow::Result<()> {
    let seq: (Option<String>,) = sqlx::query_as("SELECT pg_get_serial_sequence($1, 'id')")
        .bind(quote_ident(table))
        .fetch_one(&mut **tx)
        .await?;
    if let Some(seq) = seq.0 {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT setval('{seq}', (SELECT COALESCE(MAX(id), 0) + 1 FROM {}), false)",
            quote_ident(table)
        )))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

#[cfg(feature = "postgres")]
type PgQuery<'q> = sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>;

/// Bind one JSON value as the target column's PostgreSQL type. SQLite stores
/// booleans as 0/1 and every timestamp as text, so those convert here.
#[cfg(feature = "postgres")]
fn bind_postgres<'q>(
    q: PgQuery<'q>,
    value: &'q Value,
    data_type: &str,
) -> anyhow::Result<PgQuery<'q>> {
    Ok(match data_type {
        "boolean" => q.bind(match value {
            Value::Null => None,
            Value::Bool(b) => Some(*b),
            Value::Number(n) => Some(n.as_i64().unwrap_or(0) != 0),
            Value::String(s) => Some(matches!(s.as_str(), "true" | "t" | "1")),
            other => anyhow::bail!("cannot store {other} as boolean"),
        }),
        "bigint" => q.bind(json_i64(value)?),
        "integer" => q.bind(json_i64(value)?.map(|v| v as i32)),
        "smallint" => q.bind(json_i64(value)?.map(|v| v as i16)),
        "double precision" => q.bind(json_f64(value)?),
        "real" => q.bind(json_f64(value)?.map(|v| v as f32)),
        "timestamp with time zone" => q.bind(match value {
            Value::Null => None,
            Value::String(s) => Some(parse_timestamp(s)?),
            other => anyhow::bail!("cannot store {other} as a timestamp"),
        }),
        "timestamp without time zone" => q.bind(match value {
            Value::Null => None,
            Value::String(s) => Some(parse_timestamp(s)?.naive_utc()),
            other => anyhow::bail!("cannot store {other} as a timestamp"),
        }),
        "bytea" => q.bind(match value {
            Value::Null => None,
            other => Some(
                json_to_bytes(other)
                    .ok_or_else(|| anyhow::anyhow!("expected {{\"$bytes\": ...}}"))?,
            ),
        }),
        _ => q.bind(match value {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            Value::Bool(b) => Some(b.to_string()),
            Value::Number(n) => Some(n.to_string()),
            other => Some(other.to_string()),
        }),
    })
}

#[cfg(feature = "postgres")]
fn json_i64(value: &Value) -> anyhow::Result<Option<i64>> {
    Ok(match value {
        Value::Null => None,
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => Some(
            n.as_i64()
                .or_else(|| n.as_f64().map(|f| f as i64))
                .ok_or_else(|| anyhow::anyhow!("{n} does not fit an integer"))?,
        ),
        Value::String(s) => Some(s.parse()?),
        other => anyhow::bail!("cannot store {other} as an integer"),
    })
}

#[cfg(feature = "postgres")]
fn json_f64(value: &Value) -> anyhow::Result<Option<f64>> {
    Ok(match value {
        Value::Null => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => Some(s.parse()?),
        other => anyhow::bail!("cannot store {other} as a float"),
    })
}

/// RFC 3339, or SQLite's `datetime('now')` shape, read as UTC.
#[cfg(feature = "postgres")]
fn parse_timestamp(s: &str) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&chrono::Utc));
    }
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f"))
        .map_err(|e| anyhow::anyhow!("{s:?} is not a timestamp: {e}"))?;
    Ok(naive.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::MainDb;
    use crate::db::models::{NewApiKey, NewBudgetRule, NewUser};
    use crate::db::repositories::{ApiKeyRepository, BudgetRepository, UserRepository};
    use crate::db::sqlite::SqliteDb;
    use std::sync::Arc;

    async fn migrated() -> MainDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        crate::db::migrations::run_migrations(&db.pool)
            .await
            .unwrap();
        let reports = ReportPool::from(&db.pool);
        MainDb { db: Arc::new(db), reports }
    }

    async fn seed(store: &MainDb) -> i64 {
        let user = UserRepository::create(
            &*store.db,
            NewUser {
                name: "alice".into(),
                email: Some("a@example.test".into()),
            },
        )
        .await
        .unwrap();
        ApiKeyRepository::create_api_key(
            &*store.db,
            NewApiKey {
                user_id: user.id,
                key_hash: "ab".repeat(32),
                label: Some("laptop".into()),
                expires_at: None,
                project: Some("p1".into()),
                session_window_secs: None,
            },
        )
        .await
        .unwrap();
        BudgetRepository::create(
            &*store.db,
            NewBudgetRule {
                user_id: Some(user.id),
                group_name: None,
                api_key_id: None,
                tag: None,
                window: "monthly".into(),
                limit_usd: Some(12.5),
                limit_tokens: None,
                model_allow: vec![],
                model_deny: vec!["x/y".into()],
                rate_rpm: Some(10),
                max_concurrent: Some(2),
                project: None,
                window_start: None,
                window_end: None,
            },
        )
        .await
        .unwrap();
        user.id
    }

    async fn count(store: &MainDb, table: &str) -> i64 {
        let ReportPool::Sqlite(pool) = &store.reports else { panic!("sqlite store") };
        let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap();
        n
    }

    #[tokio::test]
    async fn sqlite_round_trip_preserves_rows_ids_and_hashes() {
        let source = migrated().await;
        let user_id = seed(&source).await;
        let dump = export(&source.reports).await.unwrap();
        assert_eq!(dump.format_version, FORMAT_VERSION);
        assert_eq!(dump.source_backend, "sqlite");
        assert!(!dump.tables.contains_key("_sqlx_migrations"));
        assert_eq!(dump.tables["users"].rows.len(), 1);
        assert_eq!(dump.tables["api_keys"].rows.len(), 1);
        assert_eq!(dump.tables["budget_rules"].rows.len(), 1);
        assert!(
            dump.tables.len() > 20,
            "every table is exported, {} found",
            dump.tables.len()
        );

        // JSON is the wire format: go through it.
        let json = serde_json::to_string(&dump).unwrap();
        let parsed: Dump = serde_json::from_str(&json).unwrap();

        let target = migrated().await;
        let reports = import(&target.reports, &parsed, false).await.unwrap();
        let users = reports.iter().find(|r| r.table == "users").unwrap();
        assert_eq!((users.inserted, users.total_after), (1, 1));
        assert_eq!(count(&target, "api_keys").await, 1);
        assert_eq!(count(&target, "budget_rules").await, 1);

        let moved = ApiKeyRepository::find_api_key_by_hash(&*target.db, &"ab".repeat(32))
            .await
            .unwrap()
            .expect("key hash moved as stored");
        assert_eq!(moved.user_id, user_id);
        let rules = BudgetRepository::list_for_user(&*target.db, user_id)
            .await
            .unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].limit_usd, Some(12.5));
        assert_eq!(rules[0].max_concurrent, Some(2));
        assert!(
            rules[0].model_deny.contains("x/y"),
            "{:?}",
            rules[0].model_deny
        );

        // Ids were preserved, and the sequence moved past them.
        let next = UserRepository::create(
            &*target.db,
            NewUser {
                name: "bob".into(),
                email: None,
            },
        )
        .await
        .unwrap();
        assert!(next.id > user_id);
    }

    #[tokio::test]
    async fn import_refuses_a_non_empty_target_unless_merge() {
        let source = migrated().await;
        seed(&source).await;
        let dump = export(&source.reports).await.unwrap();

        let target = migrated().await;
        import(&target.reports, &dump, false).await.unwrap();
        let err = import(&target.reports, &dump, false).await.unwrap_err().to_string();
        assert!(
            err.contains("already holds rows") && err.contains("users (1)"),
            "{err}"
        );
        assert_eq!(count(&target, "users").await, 1, "nothing was written");

        // With --merge the same ids collide and the whole import rolls back.
        let err = import(&target.reports, &dump, true).await.unwrap_err().to_string();
        assert!(err.contains("users"), "{err}");
        assert_eq!(count(&target, "api_keys").await, 1);
    }

    #[tokio::test]
    async fn import_rejects_a_source_column_the_target_lacks() {
        let source = migrated().await;
        seed(&source).await;
        let mut dump = export(&source.reports).await.unwrap();
        let users = dump.tables.get_mut("users").unwrap();
        users.columns.push("from_the_future".into());
        for row in &mut users.rows {
            row.push(Value::from(1));
        }
        let target = migrated().await;
        let err = import(&target.reports, &dump, false).await.unwrap_err().to_string();
        assert!(
            err.contains("users") && err.contains("from_the_future"),
            "{err}"
        );
        assert_eq!(count(&target, "users").await, 0);
    }

    #[tokio::test]
    async fn import_rejects_an_unknown_format_version() {
        let target = migrated().await;
        let dump = Dump {
            format_version: FORMAT_VERSION + 1,
            source_backend: "sqlite".into(),
            exported_at: String::new(),
            tables: BTreeMap::new(),
        };
        let err = import(&target.reports, &dump, false).await.unwrap_err().to_string();
        assert!(err.contains("format version"), "{err}");
    }

    #[test]
    fn known_tables_import_parent_first_and_unknown_ones_last() {
        let mut dump = Dump {
            format_version: FORMAT_VERSION,
            source_backend: "sqlite".into(),
            exported_at: String::new(),
            tables: BTreeMap::new(),
        };
        for t in ["cost_ledger", "zz_new_table", "users", "api_keys"] {
            dump.tables.insert(t.into(), TableDump::default());
        }
        assert_eq!(
            ordered_tables(&dump),
            vec!["users", "api_keys", "cost_ledger", "zz_new_table"]
        );
    }

    #[test]
    fn bytes_survive_json() {
        let v = bytes_to_json(&[0, 1, 2, 255]);
        assert_eq!(json_to_bytes(&v).unwrap(), vec![0, 1, 2, 255]);
        assert!(json_to_bytes(&Value::from("plain")).is_none());
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn timestamps_parse_in_both_shapes_sqlite_writes() {
        assert_eq!(
            parse_timestamp("2026-03-01T00:00:00+00:00")
                .unwrap()
                .to_rfc3339(),
            "2026-03-01T00:00:00+00:00"
        );
        assert_eq!(
            parse_timestamp("2026-03-01 12:34:56").unwrap().to_rfc3339(),
            "2026-03-01T12:34:56+00:00"
        );
        assert!(parse_timestamp("yesterday").is_err());
    }
}
