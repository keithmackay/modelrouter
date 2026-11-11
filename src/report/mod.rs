pub mod formatter;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The database a report reads. The report SQL is written once, in the
/// subset both backends accept (`?` placeholders are renumbered for
/// Postgres, sums are cast to `BIGINT`, day buckets use `substr`).
#[derive(Clone)]
pub enum ReportPool {
    Sqlite(sqlx::SqlitePool),
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgPool),
}

impl From<sqlx::SqlitePool> for ReportPool {
    fn from(pool: sqlx::SqlitePool) -> Self {
        ReportPool::Sqlite(pool)
    }
}

impl From<&sqlx::SqlitePool> for ReportPool {
    fn from(pool: &sqlx::SqlitePool) -> Self {
        ReportPool::Sqlite(pool.clone())
    }
}

impl From<&ReportPool> for ReportPool {
    fn from(pool: &ReportPool) -> Self {
        pool.clone()
    }
}

#[cfg(feature = "postgres")]
impl From<sqlx::PgPool> for ReportPool {
    fn from(pool: sqlx::PgPool) -> Self {
        ReportPool::Postgres(pool)
    }
}

/// A bound report parameter.
enum Arg {
    Text(String),
    Int(i64),
}

/// A row type every enabled backend can decode.
#[cfg(not(feature = "postgres"))]
trait ReportRow: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin {}
#[cfg(not(feature = "postgres"))]
impl<T> ReportRow for T where T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin {}

#[cfg(feature = "postgres")]
trait ReportRow:
    for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow>
    + for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>
    + Send
    + Unpin
{
}
#[cfg(feature = "postgres")]
impl<T> ReportRow for T where
    T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow>
        + for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>
        + Send
        + Unpin
{
}

impl ReportPool {
    /// `"sqlite"` or `"postgres"`.
    pub fn backend(&self) -> &'static str {
        match self {
            ReportPool::Sqlite(_) => "sqlite",
            #[cfg(feature = "postgres")]
            ReportPool::Postgres(_) => "postgres",
        }
    }

    async fn fetch_all<R: ReportRow>(&self, sql: &str, args: &[Arg]) -> Result<Vec<R>> {
        match self {
            ReportPool::Sqlite(pool) => {
                // sqlx 0.9 wants an owned or 'static statement; the SQL is built by this module.
                let mut q = sqlx::query_as::<_, R>(sqlx::AssertSqlSafe(sql.to_string()));
                for arg in args {
                    q = match arg {
                        Arg::Text(v) => q.bind(v.clone()),
                        Arg::Int(v) => q.bind(*v),
                    };
                }
                Ok(q.fetch_all(pool).await?)
            }
            #[cfg(feature = "postgres")]
            ReportPool::Postgres(pool) => {
                let sql = numbered_placeholders(sql);
                let mut q = sqlx::query_as::<_, R>(sqlx::AssertSqlSafe(sql));
                for arg in args {
                    q = match arg {
                        Arg::Text(v) => q.bind(v.clone()),
                        Arg::Int(v) => q.bind(*v),
                    };
                }
                Ok(q.fetch_all(pool).await?)
            }
        }
    }
}

/// Rewrite `?` placeholders as `$1`, `$2`, … for Postgres, leaving any `?`
/// inside a single-quoted literal alone.
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
fn numbered_placeholders(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = 0;
    let mut in_quote = false;
    for c in sql.chars() {
        match c {
            '\'' => {
                in_quote = !in_quote;
                out.push(c);
            }
            '?' if !in_quote => {
                n += 1;
                out.push('$');
                out.push_str(&n.to_string());
            }
            _ => out.push(c),
        }
    }
    out
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CostRow {
    pub user_name: String,
    pub model: String,
    pub window: String,
    pub groups: String,
    pub project: String,
    pub key: String,
    pub total_cost_usd: f64,
    /// Avoided by response-cache hits.
    pub saved_usd: f64,
    /// `total_cost_usd + saved_usd`: the spend with no response cache.
    pub uncached_cost_usd: f64,
    pub request_count: i64,
    pub total_tokens_out: i64,
    pub total_tokens_in: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UsageRow {
    pub model: String,
    pub provider: String,
    pub request_count: i64,
    pub total_tokens_in: i64,
    pub total_tokens_out: i64,
    pub total_cost_usd: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PromptRow {
    pub id: i64,
    pub user_name: String,
    pub request_model: String,
    pub routed_model: String,
    pub cost_usd: f64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// True when any tokens were served from the provider's prompt cache.
    pub cached: bool,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuditRow {
    pub id: i64,
    pub actor_name: String,
    pub action: String,
    pub target: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HookStats {
    pub hook_name: String,
    pub invocation_count: i64,
    pub success_rate: f64,
    pub avg_duration_ms: f64,
    pub p50_duration_ms: i64,
    pub p95_duration_ms: i64,
    pub p99_duration_ms: i64,
}

pub async fn cost_by_user_window(
    pool: impl Into<ReportPool>,
    window: &str,
    user_name: Option<&str>,
    project: Option<&str>,
    group: Option<&str>,
    model: Option<&str>,
    key_id: Option<i64>,
) -> Result<Vec<CostRow>> {
    let pool: ReportPool = pool.into();
    let window_start = window_start_str(window)?;

    // ── Resolve group → active member user_ids ────────────────────────────
    let group_user_ids: Option<Vec<i64>> = if let Some(gname) = group {
        let ids: Vec<(i64,)> = pool
            .fetch_all(
                "SELECT gm.user_id FROM group_memberships gm
                 JOIN groups g ON gm.group_id = g.id
                 WHERE g.name = ? AND gm.disabled_at IS NULL",
                &[Arg::Text(gname.to_string())],
            )
            .await?;
        Some(ids.into_iter().map(|(id,)| id).collect())
    } else {
        None
    };

    // Empty group → no results
    if let Some(ref ids) = group_user_ids {
        if ids.is_empty() {
            return Ok(vec![]);
        }
    }

    // ── Pre-load user_id → groups map (active memberships) ────────────────
    let membership_rows: Vec<(i64, String)> = pool
        .fetch_all(
            "SELECT gm.user_id, g.name
             FROM group_memberships gm
             JOIN groups g ON gm.group_id = g.id
             WHERE gm.disabled_at IS NULL",
            &[],
        )
        .await
        .unwrap_or_default();
    let mut user_group_map: std::collections::HashMap<i64, Vec<String>> =
        std::collections::HashMap::new();
    for (uid, gname) in membership_rows {
        user_group_map.entry(uid).or_default().push(gname);
    }

    // ── Build query ────────────────────────────────────────────────────────
    let mut sql = "SELECT u.id, u.name, cl.model, \
                          COALESCE(cl.project, '') as project, \
                          cl.api_key_id, \
                          COALESCE(ak.project, '') as key_project, \
                          COALESCE(ak.label, '') as key_label, \
                          COALESCE(SUM(cl.cost_usd), 0.0), \
                          COALESCE(SUM(cl.saved_usd), 0.0), \
                          CAST(COALESCE(SUM(cl.tokens_in), 0) AS BIGINT), \
                          CAST(COALESCE(SUM(cl.tokens_out), 0) AS BIGINT), \
                          COUNT(*) \
                   FROM cost_ledger cl \
                   JOIN users u ON cl.user_id = u.id \
                   LEFT JOIN api_keys ak ON cl.api_key_id = ak.id \
                   WHERE cl.created_at >= ?"
        .to_string();

    if user_name.is_some() { sql.push_str(" AND u.name = ?"); }
    if project.is_some()   { sql.push_str(" AND cl.project = ?"); }
    if key_id.is_some()    { sql.push_str(" AND cl.api_key_id = ?"); }
    if let Some(ref ids) = group_user_ids {
        let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        sql.push_str(&format!(" AND cl.user_id IN ({})", list));
    }
    if model.is_some() { sql.push_str(" AND cl.model = ?"); }

    // The key's project and label are functionally dependent on api_key_id,
    // but Postgres still requires every selected column to be grouped.
    sql.push_str(" GROUP BY u.id, u.name, cl.model, cl.project, cl.api_key_id, ak.project, ak.label \
                   ORDER BY SUM(cl.cost_usd) DESC");

    type Row = (i64, String, String, String, Option<i64>, String, String, f64, f64, i64, i64, i64);
    let mut args = vec![Arg::Text(window_start)];
    if let Some(v) = user_name { args.push(Arg::Text(v.to_string())); }
    if let Some(v) = project   { args.push(Arg::Text(v.to_string())); }
    if let Some(v) = key_id    { args.push(Arg::Int(v)); }
    if let Some(v) = model     { args.push(Arg::Text(v.to_string())); }

    let rows: Vec<Row> = pool.fetch_all(&sql, &args).await?;
    Ok(rows
        .into_iter()
        .map(|(uid, uname, mdl, proj, _api_key_id, key_proj, key_lbl, cost, saved, tin, tout, cnt)| {
            let groups_str = user_group_map.get(&uid)
                .map(|gs| gs.join(", "))
                .unwrap_or_default();
            let key_str = if key_lbl.is_empty() && key_proj.is_empty() {
                String::new()
            } else if key_lbl.is_empty() {
                key_proj.clone()
            } else {
                format!("{} ({})", key_proj, key_lbl)
            };
            CostRow {
                user_name: uname,
                model: mdl,
                window: window.to_string(),
                groups: groups_str,
                project: proj,
                key: key_str,
                total_cost_usd: cost,
                saved_usd: saved,
                uncached_cost_usd: cost + saved,
                request_count: cnt,
                total_tokens_out: tout,
                total_tokens_in: tin,
            }
        })
        .collect())
}

pub async fn usage_by_model(
    pool: impl Into<ReportPool>,
    since: Option<&str>,
    model: Option<&str>,
    project: Option<&str>,
) -> Result<Vec<UsageRow>> {
    let since = since.unwrap_or("1970-01-01T00:00:00Z");

    // Build dynamic query based on optional filters
    // cost_ledger has a `project` column directly, no join needed
    let mut conditions = vec!["created_at >= ?"];
    if model.is_some() {
        conditions.push("model = ?");
    }
    if project.is_some() {
        conditions.push("project = ?");
    }
    let where_clause = conditions.join(" AND ");

    let base_query = format!(
        r#"SELECT model, provider,
                  COUNT(*) as request_count,
                  CAST(COALESCE(SUM(tokens_in), 0) AS BIGINT) as tokens_in,
                  CAST(COALESCE(SUM(tokens_out), 0) AS BIGINT) as tokens_out,
                  COALESCE(SUM(cost_usd), 0.0) as total_cost
           FROM cost_ledger
           WHERE {}
           GROUP BY model, provider
           ORDER BY total_cost DESC"#,
        where_clause
    );

    let mut args = vec![Arg::Text(since.to_string())];
    if let Some(m) = model {
        args.push(Arg::Text(m.to_string()));
    }
    if let Some(p) = project {
        args.push(Arg::Text(p.to_string()));
    }

    let rows: Vec<(String, String, i64, i64, i64, f64)> =
        pool.into().fetch_all(&base_query, &args).await?;
    Ok(rows
        .into_iter()
        .map(
            |(model, provider, request_count, total_tokens_in, total_tokens_out, total_cost_usd)| {
                UsageRow {
                    model,
                    provider,
                    request_count,
                    total_tokens_in,
                    total_tokens_out,
                    total_cost_usd,
                }
            },
        )
        .collect())
}

pub async fn recent_prompts(
    pool: impl Into<ReportPool>,
    user_name: Option<&str>,
    limit: u32,
    since: Option<&str>,
) -> Result<Vec<PromptRow>> {
    let since = since.unwrap_or("1970-01-01T00:00:00Z");
    let mut sql = String::from(
        "SELECT p.id, u.name, p.request_model, p.routed_model, \
                p.cost_usd, p.prompt_tokens, p.completion_tokens, \
                p.cache_read_tokens, p.cache_write_tokens, p.created_at \
         FROM prompts p \
         JOIN users u ON p.user_id = u.id \
         WHERE p.created_at >= ?",
    );
    let mut args = vec![Arg::Text(since.to_string())];
    if let Some(name) = user_name {
        sql.push_str(" AND u.name = ?");
        args.push(Arg::Text(name.to_string()));
    }
    sql.push_str(" ORDER BY p.created_at DESC LIMIT ?");
    args.push(Arg::Int(limit as i64));
    #[allow(clippy::type_complexity)]
    let rows: Vec<(i64, String, String, String, f64, i64, i64, i64, i64, String)> =
        pool.into().fetch_all(&sql, &args).await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, user_name, request_model, routed_model, cost_usd, prompt_tokens, completion_tokens, cache_read_tokens, cache_write_tokens, created_at)| {
                PromptRow {
                    id,
                    user_name,
                    request_model,
                    routed_model,
                    cost_usd,
                    prompt_tokens,
                    completion_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    cached: cache_read_tokens > 0,
                    created_at,
                }
            },
        )
        .collect())
}

pub async fn recent_audit(
    pool: impl Into<ReportPool>,
    actor_name: Option<&str>,
    limit: u32,
) -> Result<Vec<AuditRow>> {
    let mut sql = String::from("SELECT id, actor_name, action, target, created_at FROM audit_log");
    let mut args = Vec::new();
    if let Some(actor) = actor_name {
        sql.push_str(" WHERE actor_name = ?");
        args.push(Arg::Text(actor.to_string()));
    }
    sql.push_str(" ORDER BY created_at DESC LIMIT ?");
    args.push(Arg::Int(limit as i64));
    let rows: Vec<(i64, String, String, Option<String>, String)> =
        pool.into().fetch_all(&sql, &args).await?;
    Ok(rows
        .into_iter()
        .map(|(id, actor_name, action, target, created_at)| AuditRow {
            id,
            actor_name,
            action,
            target,
            created_at,
        })
        .collect())
}

pub async fn hook_latency_stats(pool: impl Into<ReportPool>) -> Result<Vec<HookStats>> {
    // Single query: fetch all rows sorted by hook_name, duration_ms
    // This avoids N+1 queries and lets us group in Rust. `success` is an
    // integer in SQLite and a BOOLEAN in Postgres; the CASE reads both.
    let raw: Vec<(String, i64, i64)> = pool
        .into()
        .fetch_all(
            "SELECT hook_name, duration_ms, \
                    CAST(CASE WHEN success THEN 1 ELSE 0 END AS BIGINT) \
             FROM hook_metrics \
             ORDER BY hook_name, duration_ms",
            &[],
        )
        .await?;

    // Group durations and aggregate per hook name
    let mut durations_map: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut success_counts: BTreeMap<String, (i64, i64)> = BTreeMap::new(); // (successes, total)
    let mut sum_map: BTreeMap<String, i64> = BTreeMap::new();

    for (hook_name, duration_ms, success) in &raw {
        durations_map
            .entry(hook_name.clone())
            .or_default()
            .push(*duration_ms);
        let entry = success_counts.entry(hook_name.clone()).or_insert((0, 0));
        entry.0 += *success;
        entry.1 += 1;
        *sum_map.entry(hook_name.clone()).or_insert(0) += duration_ms;
    }

    fn percentile(sorted: &[i64], pct: usize) -> i64 {
        let n = sorted.len();
        if n == 0 {
            return 0;
        }
        // Use nearest-rank method: ceil(p/100 * n) - 1, clamped to [0, n-1]
        let idx = (pct * n).div_ceil(100).saturating_sub(1).min(n - 1);
        sorted[idx]
    }

    let mut result = Vec::new();
    for (hook_name, durations) in &durations_map {
        let n = durations.len() as i64;
        let (successes, total) = success_counts[hook_name];
        let sum = sum_map[hook_name];
        let avg_duration_ms = if total > 0 { sum as f64 / total as f64 } else { 0.0 };
        let success_rate = if total > 0 { successes as f64 / total as f64 } else { 0.0 };

        result.push(HookStats {
            hook_name: hook_name.clone(),
            invocation_count: n,
            success_rate,
            avg_duration_ms,
            p50_duration_ms: percentile(durations, 50),
            p95_duration_ms: percentile(durations, 95),
            p99_duration_ms: percentile(durations, 99),
        });
    }
    Ok(result)
}

// ── Usage report types ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum UsageScope {
    Global,
    Group(String),
    Project(String),
    User(String),
    UserInGroup { user: String, group: String },
    UserInProject { user: String, project: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum UsageWindow {
    AllTime,
    Annual,
    Monthly,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UsageGranularity {
    Total,
    Subtotal,
    Detail,
}

#[derive(Debug, Clone)]
pub struct UsageReportRow {
    pub bucket: Option<String>,
    pub user_name: Option<String>,
    pub project: Option<String>,
    pub model: Option<String>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost_usd: f64,
    /// Avoided by response-cache hits.
    pub saved_usd: f64,
}

impl UsageReportRow {
    /// The spend with no response cache.
    pub fn uncached_cost_usd(&self) -> f64 {
        self.cost_usd + self.saved_usd
    }
}

pub async fn fetch_usage_rows(
    pool: impl Into<ReportPool>,
    scope: &UsageScope,
    window: &UsageWindow,
    granularity: &UsageGranularity,
) -> Result<Vec<UsageReportRow>> {
    // Build bucket expression
    let bucket_expr = match window {
        UsageWindow::AllTime => "NULL".to_string(),
        // created_at is RFC 3339 text, so the year and month are prefixes.
        UsageWindow::Annual => "substr(cl.created_at, 1, 4)".to_string(),
        UsageWindow::Monthly => "substr(cl.created_at, 1, 7)".to_string(),
    };

    // Build SELECT fields based on granularity
    let select_fields = match granularity {
        UsageGranularity::Total => format!(
            "{} as bucket, NULL as user_name, NULL as project_col, NULL as model_col, \
             CAST(COALESCE(SUM(cl.tokens_in), 0) AS BIGINT) as tokens_in, \
             CAST(COALESCE(SUM(cl.tokens_out), 0) AS BIGINT) as tokens_out, \
             COALESCE(SUM(cl.cost_usd), 0.0) as cost_usd, \
             COALESCE(SUM(cl.saved_usd), 0.0) as saved_usd",
            bucket_expr
        ),
        UsageGranularity::Subtotal => format!(
            "{} as bucket, u.name as user_name, cl.project as project_col, NULL as model_col, \
             CAST(COALESCE(SUM(cl.tokens_in), 0) AS BIGINT) as tokens_in, \
             CAST(COALESCE(SUM(cl.tokens_out), 0) AS BIGINT) as tokens_out, \
             COALESCE(SUM(cl.cost_usd), 0.0) as cost_usd, \
             COALESCE(SUM(cl.saved_usd), 0.0) as saved_usd",
            bucket_expr
        ),
        UsageGranularity::Detail => format!(
            "{} as bucket, u.name as user_name, cl.project as project_col, cl.model as model_col, \
             cl.tokens_in as tokens_in, cl.tokens_out as tokens_out, cl.cost_usd, cl.saved_usd",
            bucket_expr
        ),
    };

    // Build JOIN and WHERE clause
    let (joins, where_parts, bind_values) = match scope {
        UsageScope::Global => (
            "JOIN users u ON cl.user_id = u.id".to_string(),
            vec![],
            vec![],
        ),
        UsageScope::Group(gname) => (
            "JOIN users u ON cl.user_id = u.id \
             JOIN group_memberships gm ON gm.user_id = cl.user_id \
                 AND gm.joined_at <= cl.created_at \
                 AND (gm.disabled_at IS NULL OR cl.created_at < gm.disabled_at) \
             JOIN groups g ON g.id = gm.group_id".to_string(),
            vec!["g.name = ?"],
            vec![gname.clone()],
        ),
        UsageScope::Project(proj) => (
            "JOIN users u ON cl.user_id = u.id".to_string(),
            vec!["cl.project = ?"],
            vec![proj.clone()],
        ),
        UsageScope::User(uname) => (
            "JOIN users u ON cl.user_id = u.id".to_string(),
            vec!["u.name = ?"],
            vec![uname.clone()],
        ),
        UsageScope::UserInGroup { user, group } => (
            "JOIN users u ON cl.user_id = u.id \
             JOIN group_memberships gm ON gm.user_id = cl.user_id \
                 AND gm.joined_at <= cl.created_at \
                 AND (gm.disabled_at IS NULL OR cl.created_at < gm.disabled_at) \
             JOIN groups g ON g.id = gm.group_id".to_string(),
            vec!["u.name = ?", "g.name = ?"],
            vec![user.clone(), group.clone()],
        ),
        UsageScope::UserInProject { user, project } => (
            "JOIN users u ON cl.user_id = u.id".to_string(),
            vec!["u.name = ?", "cl.project = ?"],
            vec![user.clone(), project.clone()],
        ),
    };

    let where_clause = if where_parts.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_parts.join(" AND "))
    };

    // Build GROUP BY
    let group_by = match granularity {
        UsageGranularity::Total => match window {
            UsageWindow::AllTime => String::new(),
            _ => "GROUP BY bucket".to_string(),
        },
        UsageGranularity::Subtotal => {
            let mut cols = vec![];
            if *window != UsageWindow::AllTime { cols.push("bucket"); }
            cols.push("u.name");
            cols.push("cl.project");
            format!("GROUP BY {}", cols.join(", "))
        },
        UsageGranularity::Detail => {
            let mut cols = vec![];
            if *window != UsageWindow::AllTime { cols.push("bucket"); }
            cols.push("u.name");
            cols.push("cl.project");
            cols.push("cl.model");
            cols.push("cl.id");
            format!("GROUP BY {}", cols.join(", "))
        },
    };

    // Build ORDER BY
    let order_by = "ORDER BY bucket ASC, user_name ASC, project_col ASC, model_col ASC";

    let sql = format!(
        "SELECT {select_fields} FROM cost_ledger cl {joins} {where_clause} {group_by} {order_by}"
    );

    let args: Vec<Arg> = bind_values.into_iter().map(Arg::Text).collect();
    #[allow(clippy::type_complexity)]
    let rows: Vec<(Option<String>, Option<String>, Option<String>, Option<String>, i64, i64, f64, f64)> =
        pool.into().fetch_all(&sql, &args).await?;
    Ok(rows
        .into_iter()
        .map(
            |(bucket, user_name, project, model, tokens_in, tokens_out, cost_usd, saved_usd)| {
                UsageReportRow {
                    bucket,
                    user_name,
                    project,
                    model,
                    tokens_in,
                    tokens_out,
                    cost_usd,
                    saved_usd,
                }
            },
        )
        .collect())
}

pub fn window_start_str(window: &str) -> Result<String> {
    use chrono::Datelike;
    let now = chrono::Utc::now();
    match window {
        "daily" => Ok(now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .to_rfc3339()),
        "weekly" => {
            let days = now.weekday().num_days_from_monday() as i64;
            Ok((now - chrono::Duration::days(days))
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .to_rfc3339())
        }
        "monthly" => Ok(now
            .with_day(1)
            .unwrap()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .to_rfc3339()),
        "alltime" => Ok("1970-01-01T00:00:00Z".to_string()),
        other => Err(anyhow::anyhow!(
            "invalid window '{}': expected daily, weekly, monthly, or alltime",
            other
        )),
    }
}
