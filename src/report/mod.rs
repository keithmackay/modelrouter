pub mod formatter;

use anyhow::Result;
use serde::{Deserialize, Serialize};

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
}

pub async fn cost_by_user_window(
    pool: &sqlx::SqlitePool,
    window: &str,
    user_name: Option<&str>,
) -> Result<Vec<CostRow>> {
    let window_start = window_start_str(window);

    if let Some(name) = user_name {
        let rows = sqlx::query_as::<_, (String, String, f64, i64, i64, i64)>(
            r#"SELECT u.name, cl.model,
                      COALESCE(SUM(cl.cost_usd), 0.0) as total_cost,
                      COALESCE(SUM(cl.tokens_in), 0) as tokens_in,
                      COALESCE(SUM(cl.tokens_out), 0) as tokens_out,
                      COUNT(*) as request_count
               FROM cost_ledger cl
               JOIN users u ON cl.user_id = u.id
               WHERE cl.created_at >= ? AND u.name = ?
               GROUP BY u.name, cl.model
               ORDER BY total_cost DESC"#,
        )
        .bind(&window_start)
        .bind(name)
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(user_name, model, total_cost_usd, total_tokens_in, total_tokens_out, request_count)| {
                    CostRow {
                        user_name,
                        model,
                        total_cost_usd,
                        total_tokens_in,
                        total_tokens_out,
                        request_count,
                    }
                },
            )
            .collect())
    } else {
        let rows = sqlx::query_as::<_, (String, String, f64, i64, i64, i64)>(
            r#"SELECT u.name, cl.model,
                      COALESCE(SUM(cl.cost_usd), 0.0) as total_cost,
                      COALESCE(SUM(cl.tokens_in), 0) as tokens_in,
                      COALESCE(SUM(cl.tokens_out), 0) as tokens_out,
                      COUNT(*) as request_count
               FROM cost_ledger cl
               JOIN users u ON cl.user_id = u.id
               WHERE cl.created_at >= ?
               GROUP BY u.name, cl.model
               ORDER BY total_cost DESC"#,
        )
        .bind(&window_start)
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(user_name, model, total_cost_usd, total_tokens_in, total_tokens_out, request_count)| {
                    CostRow {
                        user_name,
                        model,
                        total_cost_usd,
                        total_tokens_in,
                        total_tokens_out,
                        request_count,
                    }
                },
            )
            .collect())
    }
}

pub async fn usage_by_model(
    pool: &sqlx::SqlitePool,
    since: Option<&str>,
) -> Result<Vec<UsageRow>> {
    let since = since.unwrap_or("1970-01-01T00:00:00Z");
    let rows = sqlx::query_as::<_, (String, String, i64, i64, i64, f64)>(
        r#"SELECT model, provider,
                  COUNT(*) as request_count,
                  COALESCE(SUM(tokens_in), 0) as tokens_in,
                  COALESCE(SUM(tokens_out), 0) as tokens_out,
                  COALESCE(SUM(cost_usd), 0.0) as total_cost
           FROM cost_ledger
           WHERE created_at >= ?
           GROUP BY model, provider
           ORDER BY total_cost DESC"#,
    )
    .bind(since)
    .fetch_all(pool)
    .await?;
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
    let stats = sqlx::query_as::<_, (String, i64, f64, f64)>(
        r#"SELECT hook_name,
                  COUNT(*) as invocation_count,
                  AVG(CAST(success AS REAL)) as success_rate,
                  AVG(CAST(duration_ms AS REAL)) as avg_duration_ms
           FROM hook_metrics
           GROUP BY hook_name"#,
    )
    .fetch_all(pool)
    .await?;

    let mut result = Vec::new();
    for (hook_name, invocation_count, success_rate, avg_duration_ms) in stats {
        let all_durations: Vec<(i64,)> = sqlx::query_as(
            "SELECT duration_ms FROM hook_metrics WHERE hook_name = ? ORDER BY duration_ms",
        )
        .bind(&hook_name)
        .fetch_all(pool)
        .await?;

        let n = all_durations.len();
        let p50 = if n > 0 { all_durations[n * 50 / 100].0 } else { 0 };
        let p95 = if n > 0 { all_durations[(n * 95 / 100).min(n - 1)].0 } else { 0 };

        result.push(HookStats {
            hook_name,
            invocation_count,
            success_rate,
            avg_duration_ms,
            p50_duration_ms: p50,
            p95_duration_ms: p95,
        });
    }
    Ok(result)
}

fn window_start_str(window: &str) -> String {
    use chrono::Datelike;
    let now = chrono::Utc::now();
    match window {
        "daily" => now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .to_rfc3339(),
        "weekly" => {
            let days = now.weekday().num_days_from_monday() as i64;
            (now - chrono::Duration::days(days))
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .to_rfc3339()
        }
        _ => now
            .with_day(1)
            .unwrap()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .to_rfc3339(),
    }
}
