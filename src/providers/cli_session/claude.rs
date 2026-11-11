//! `claude -p`: Claude Code's print mode, one JSON result object per call.

use serde_json::Value;

use super::{Invocation, Reply};

/// Without a caller system prompt Claude Code would use its own coding-agent
/// prompt; a neutral one keeps the call close to a plain API request.
pub(super) const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";

pub(super) fn args(inv: &Invocation) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "json",
        "--model",
        &inv.model,
        // No tools, no project or user settings, no MCP servers: the model
        // answers the prompt and nothing else runs.
        "--tools",
        "",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--system-prompt",
        inv.system.as_deref().unwrap_or(DEFAULT_SYSTEM_PROMPT),
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if let Some(id) = &inv.resume {
        args.push("--resume".into());
        args.push(id.clone());
    }
    args
}

/// The prompt goes to stdin; the system prompt travels as a flag.
pub(super) fn stdin(inv: &Invocation) -> String {
    inv.prompt.clone()
}

pub(super) fn parse(stdout: &str) -> anyhow::Result<Reply> {
    let v: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| anyhow::anyhow!("unreadable `claude` output ({e}): {}", super::truncate(stdout, 500)))?;
    if v["is_error"].as_bool() == Some(true) || v["subtype"].as_str().is_some_and(|s| s != "success") {
        let status = v["api_error_status"].as_u64().map(|s| format!(" (status {s})")).unwrap_or_default();
        return Err(super::vendor_error(format!(
            "claude reported an error{status}: {}",
            v["result"].as_str().unwrap_or("no detail")
        )));
    }
    let u = &v["usage"];
    let n = |field: &Value| field.as_u64().unwrap_or(0) as u32;
    Ok(Reply {
        session_id: v["session_id"].as_str().map(String::from),
        text: v["result"].as_str().unwrap_or_default().to_string(),
        input_tokens: n(&u["input_tokens"]),
        output_tokens: n(&u["output_tokens"]),
        cache_read_tokens: n(&u["cache_read_input_tokens"]),
        cache_write_tokens: n(&u["cache_creation_input_tokens"]),
        reasoning_tokens: u["output_tokens_details"]["thinking_tokens"].as_u64().map(|t| t as u32),
    })
}
