//! `modelrouter cache …` — operator control of the response cache.
//!
//! Cache state lives in the running server's process (and, with the Redis
//! backend, in Redis), not in the CLI's database. These commands therefore talk
//! to the JWT-gated admin REST API rather than opening the SQLite file.

use anyhow::{Context, Result};
use serde_json::json;

use crate::cli::commands::{CacheArgs, CacheCommands, CachePolicyCommands};
use crate::report::formatter::OutputFormat;

/// Obtain an admin JWT: use the supplied token, otherwise log in.
async fn resolve_token(args: &CacheArgs, client: &reqwest::Client) -> Result<String> {
    if let Some(token) = args.token.clone().filter(|t| !t.is_empty()) {
        return Ok(token);
    }
    let admin = args.admin.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "no admin token: pass --token / MODELROUTER_ADMIN_TOKEN, or --admin <name> to log in"
        )
    })?;
    let password = rpassword::prompt_password(format!("Password for {}: ", admin))?;
    let resp = client
        .post(format!("{}/admin/api/login", args.url.trim_end_matches('/')))
        .json(&json!({ "name": admin, "password": password }))
        .send()
        .await
        .with_context(|| format!("cannot reach the router at {}", args.url))?;
    if !resp.status().is_success() {
        anyhow::bail!("admin login failed ({})", resp.status());
    }
    let body: serde_json::Value = resp.json().await?;
    body["token"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("login response contained no token"))
}

async fn send(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value> {
    let mut req = client.request(method, &url).bearer_auth(token);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("request to {} failed", url))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("{} — {}", status, text);
    }
    Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
}

pub async fn run(args: CacheArgs) -> Result<()> {
    let client = reqwest::Client::new();
    let base = args.url.trim_end_matches('/').to_string();
    let token = resolve_token(&args, &client).await?;

    match args.command {
        CacheCommands::Stats { format } => {
            let stats = send(
                &client,
                reqwest::Method::GET,
                format!("{}/admin/api/cache/stats", base),
                &token,
                None,
            )
            .await?;
            if matches!(format, OutputFormat::Json) {
                println!("{}", serde_json::to_string_pretty(&stats)?);
                return Ok(());
            }
            print_stats(&stats);
        }
        CacheCommands::Purge { all, model, key } => {
            let body = if let Some(model) = model {
                json!({ "scope": "model", "model": model })
            } else if let Some(key) = key {
                json!({ "scope": "key", "key": key })
            } else if all {
                json!({ "scope": "all" })
            } else {
                json!({ "scope": "all" })
            };
            let res = send(
                &client,
                reqwest::Method::POST,
                format!("{}/admin/api/cache/purge", base),
                &token,
                Some(body),
            )
            .await?;
            println!(
                "Purged {} entries (scope: {})",
                res["removed"].as_u64().unwrap_or(0),
                res["scope"].as_str().unwrap_or("all")
            );
        }
        CacheCommands::Policy(policy_args) => match policy_args.command {
            CachePolicyCommands::Get => {
                let policy = send(
                    &client,
                    reqwest::Method::GET,
                    format!("{}/admin/api/cache/policy", base),
                    &token,
                    None,
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&policy)?);
            }
            CachePolicyCommands::Set {
                enabled,
                completions_enabled,
                max_temperature,
                completions_ttl_seconds,
                search_enabled,
                search_ttl_seconds,
            } => {
                let update = build_policy_update(
                    enabled,
                    completions_enabled,
                    max_temperature,
                    completions_ttl_seconds,
                    search_enabled,
                    search_ttl_seconds,
                );
                if update.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                    anyhow::bail!("nothing to set — pass at least one policy flag");
                }
                let policy = send(
                    &client,
                    reqwest::Method::PUT,
                    format!("{}/admin/api/cache/policy", base),
                    &token,
                    Some(update),
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&policy)?);
                println!(
                    "\nNote: runtime only. Update the [cache] section of the config file \
                     to make this survive a restart."
                );
            }
        },
    }
    Ok(())
}

/// Only the flags the operator actually passed are sent, so unspecified fields
/// keep their current value server-side.
fn build_policy_update(
    enabled: Option<bool>,
    completions_enabled: Option<bool>,
    max_temperature: Option<f64>,
    completions_ttl_seconds: Option<u64>,
    search_enabled: Option<bool>,
    search_ttl_seconds: Option<u64>,
) -> serde_json::Value {
    let mut update = serde_json::Map::new();
    if let Some(v) = enabled {
        update.insert("enabled".into(), json!(v));
    }
    if let Some(v) = completions_enabled {
        update.insert("completions_enabled".into(), json!(v));
    }
    if let Some(v) = max_temperature {
        update.insert("completions_max_temperature".into(), json!(v));
    }
    if let Some(v) = completions_ttl_seconds {
        update.insert("completions_ttl_seconds".into(), json!(v));
    }
    if let Some(v) = search_enabled {
        update.insert("search_enabled".into(), json!(v));
    }
    if let Some(v) = search_ttl_seconds {
        update.insert("search_ttl_seconds".into(), json!(v));
    }
    serde_json::Value::Object(update)
}

fn pct(v: f64) -> String {
    format!("{:.1}%", v * 100.0)
}

fn print_stats(stats: &serde_json::Value) {
    let live = &stats["live"];
    let ledger = &stats["ledger"];
    println!("Backend:        {}", live["backend"].as_str().unwrap_or("?"));
    println!(
        "Enabled:        {}   Healthy: {}",
        live["enabled"].as_bool().unwrap_or(false),
        live["healthy"].as_bool().unwrap_or(false)
    );
    println!("Entries:        {}", live["entries"].as_u64().unwrap_or(0));
    println!("Evictions:      {}", live["evictions"].as_u64().unwrap_or(0));
    println!(
        "This process:   {} hits / {} misses  ({})",
        live["hits"].as_u64().unwrap_or(0),
        live["misses"].as_u64().unwrap_or(0),
        pct(live["hit_rate"].as_f64().unwrap_or(0.0))
    );
    println!(
        "Ledger ({}d):   {} hits / {} requests  ({}), ${:.4} saved",
        ledger["window_days"].as_i64().unwrap_or(0),
        ledger["hits"].as_i64().unwrap_or(0),
        ledger["requests"].as_i64().unwrap_or(0),
        pct(ledger["hit_rate"].as_f64().unwrap_or(0.0)),
        ledger["saved_usd"].as_f64().unwrap_or(0.0)
    );

    if let Some(models) = ledger["by_model"].as_array() {
        if !models.is_empty() {
            println!("\nBy model:");
            for m in models {
                println!(
                    "  {:<32} {:>6} hits / {:>6} req  {:>7}  ${:.4}",
                    m["model"].as_str().unwrap_or("?"),
                    m["hits"].as_i64().unwrap_or(0),
                    m["requests"].as_i64().unwrap_or(0),
                    pct(m["hit_rate"].as_f64().unwrap_or(0.0)),
                    m["saved_usd"].as_f64().unwrap_or(0.0)
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::build_policy_update;

    #[test]
    fn update_only_includes_supplied_flags() {
        let update = build_policy_update(None, None, Some(0.3), None, None, None);
        let obj = update.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert_eq!(obj["completions_max_temperature"], 0.3);
    }

    #[test]
    fn update_is_empty_when_nothing_supplied() {
        let update = build_policy_update(None, None, None, None, None, None);
        assert!(update.as_object().unwrap().is_empty());
    }

    #[test]
    fn update_carries_every_flag() {
        let update = build_policy_update(
            Some(true),
            Some(false),
            Some(0.0),
            Some(60),
            Some(true),
            Some(120),
        );
        let obj = update.as_object().unwrap();
        assert_eq!(obj.len(), 6);
        assert_eq!(obj["enabled"], true);
        assert_eq!(obj["completions_enabled"], false);
        assert_eq!(obj["completions_ttl_seconds"], 60);
        assert_eq!(obj["search_enabled"], true);
        assert_eq!(obj["search_ttl_seconds"], 120);
    }
}
