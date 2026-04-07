pub mod formatter;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct CostRow {
    pub user_name: String,
    pub model: String,
    pub total_cost_usd: f64,
    pub total_tokens_in: i64,
    pub total_tokens_out: i64,
    pub request_count: i64,
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
    pool: &sqlx::SqlitePool,
    window: &str,
    user_name: Option<&str>,
    group_name: Option<&str>,
    tag: Option<&str>,
) -> Result<Vec<CostRow>> {
    let window_start = window_start_str(window)?;

    // Build WHERE extras and optional api_keys join dynamically.
    let key_join = if tag.is_some() { "JOIN api_keys ak ON cl.api_key_id = ak.id" } else { "" };

    let mut extras = String::new();
    if user_name.is_some()  { extras.push_str(" AND u.name = ?"); }
    if group_name.is_some() { extras.push_str(" AND u.group_name = ?"); }
    if tag.is_some()        { extras.push_str(" AND ak.tag = ?"); }

    let sql = format!(
        r#"SELECT u.name, cl.model,
                  COALESCE(SUM(cl.cost_usd), 0.0) as total_cost,
                  COALESCE(SUM(cl.tokens_in), 0) as tokens_in,
                  COALESCE(SUM(cl.tokens_out), 0) as tokens_out,
                  COUNT(*) as request_count
           FROM cost_ledger cl
           JOIN users u ON cl.user_id = u.id
           {key_join}
           WHERE cl.created_at >= ?{extras}
           GROUP BY u.name, cl.model
           ORDER BY total_cost DESC"#,
    );

    let mut q = sqlx::query_as::<_, (String, String, f64, i64, i64, i64)>(&sql);
    q = q.bind(&window_start);
    if let Some(v) = user_name  { q = q.bind(v); }
    if let Some(v) = group_name { q = q.bind(v); }
    if let Some(v) = tag        { q = q.bind(v); }

    let rows = q.fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|(user_name, model, total_cost_usd, total_tokens_in, total_tokens_out, request_count)| {
            CostRow { user_name, model, total_cost_usd, total_tokens_in, total_tokens_out, request_count }
        })
        .collect())
}

pub async fn usage_by_model(
    pool: &sqlx::SqlitePool,
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
                  COALESCE(SUM(tokens_in), 0) as tokens_in,
                  COALESCE(SUM(tokens_out), 0) as tokens_out,
                  COALESCE(SUM(cost_usd), 0.0) as total_cost
           FROM cost_ledger
           WHERE {}
           GROUP BY model, provider
           ORDER BY total_cost DESC"#,
        where_clause
    );

    let mut q = sqlx::query_as::<_, (String, String, i64, i64, i64, f64)>(&base_query);
    q = q.bind(since);
    if let Some(m) = model {
        q = q.bind(m);
    }
    if let Some(p) = project {
        q = q.bind(p);
    }

    let rows = q.fetch_all(pool).await?;
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
    pool: &sqlx::SqlitePool,
    user_name: Option<&str>,
    limit: u32,
    since: Option<&str>,
) -> Result<Vec<PromptRow>> {
    let since = since.unwrap_or("1970-01-01T00:00:00Z");
    if let Some(name) = user_name {
        let rows = sqlx::query_as::<_, (i64, String, String, String, f64, i64, i64, String)>(
            r#"SELECT p.id, u.name, p.request_model, p.routed_model,
                      p.cost_usd, p.prompt_tokens, p.completion_tokens, p.created_at
               FROM prompts p
               JOIN users u ON p.user_id = u.id
               WHERE u.name = ? AND p.created_at >= ?
               ORDER BY p.created_at DESC
               LIMIT ?"#,
        )
        .bind(name)
        .bind(since)
        .bind(limit as i64)
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, user_name, request_model, routed_model, cost_usd, prompt_tokens, completion_tokens, created_at)| {
                    PromptRow {
                        id,
                        user_name,
                        request_model,
                        routed_model,
                        cost_usd,
                        prompt_tokens,
                        completion_tokens,
                        created_at,
                    }
                },
            )
            .collect())
    } else {
        let rows = sqlx::query_as::<_, (i64, String, String, String, f64, i64, i64, String)>(
            r#"SELECT p.id, u.name, p.request_model, p.routed_model,
                      p.cost_usd, p.prompt_tokens, p.completion_tokens, p.created_at
               FROM prompts p
               JOIN users u ON p.user_id = u.id
               WHERE p.created_at >= ?
               ORDER BY p.created_at DESC
               LIMIT ?"#,
        )
        .bind(since)
        .bind(limit as i64)
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, user_name, request_model, routed_model, cost_usd, prompt_tokens, completion_tokens, created_at)| {
                    PromptRow {
                        id,
                        user_name,
                        request_model,
                        routed_model,
                        cost_usd,
                        prompt_tokens,
                        completion_tokens,
                        created_at,
                    }
                },
            )
            .collect())
    }
}

pub async fn recent_audit(
    pool: &sqlx::SqlitePool,
    actor_name: Option<&str>,
    limit: u32,
) -> Result<Vec<AuditRow>> {
    if let Some(actor) = actor_name {
        let rows = sqlx::query_as::<_, (i64, String, String, Option<String>, String)>(
            "SELECT id, actor_name, action, target, created_at FROM audit_log WHERE actor_name = ? ORDER BY created_at DESC LIMIT ?"
        )
        .bind(actor)
        .bind(limit as i64)
        .fetch_all(pool)
        .await?;
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
    } else {
        let rows = sqlx::query_as::<_, (i64, String, String, Option<String>, String)>(
            "SELECT id, actor_name, action, target, created_at FROM audit_log ORDER BY created_at DESC LIMIT ?"
        )
        .bind(limit as i64)
        .fetch_all(pool)
        .await?;
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
}

pub async fn hook_latency_stats(pool: &sqlx::SqlitePool) -> Result<Vec<HookStats>> {
    // Single query: fetch all rows sorted by hook_name, duration_ms
    // This avoids N+1 queries and lets us group in Rust.
    let raw: Vec<(String, i64, i64)> = sqlx::query_as(
        r#"SELECT hook_name, duration_ms, success
           FROM hook_metrics
           ORDER BY hook_name, duration_ms"#,
    )
    .fetch_all(pool)
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
        let idx = ((pct * n + 99) / 100).saturating_sub(1).min(n - 1);
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
        other => Err(anyhow::anyhow!(
            "invalid window '{}': expected daily, weekly, or monthly",
            other
        )),
    }
}
