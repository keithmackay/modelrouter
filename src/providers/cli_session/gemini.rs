//! `gemini`: Gemini CLI's headless mode (prompt on stdin, `--output-format
//! json`), one JSON object per call.

use serde_json::Value;

use super::{Invocation, Reply};

pub(super) fn args(inv: &Invocation) -> Vec<String> {
    let mut args: Vec<String> =
        vec!["--output-format".into(), "json".into(), "-m".into(), inv.model.clone()];
    if let Some(id) = &inv.resume {
        args.push("--resume".into());
        args.push(id.clone());
    }
    args
}

/// Like Codex, the system prompt rides at the top of a new session's first
/// message.
pub(super) fn stdin(inv: &Invocation) -> String {
    super::codex::stdin(inv)
}

pub(super) fn parse(stdout: &str) -> anyhow::Result<Reply> {
    let v: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| anyhow::anyhow!("unreadable `gemini` output ({e}): {}", super::truncate(stdout, 500)))?;
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        return Err(super::vendor_error(format!(
            "gemini reported an error: {}",
            err["message"].as_str().unwrap_or("no detail")
        )));
    }
    let mut reply = Reply {
        session_id: v["session_id"].as_str().map(String::from),
        text: v["response"].as_str().unwrap_or_default().to_string(),
        ..Reply::default()
    };
    // Usage is reported per model the turn touched; sum them.
    let mut reasoning = None;
    if let Some(models) = v["stats"]["models"].as_object() {
        for m in models.values() {
            let t = &m["tokens"];
            let n = |field: &Value| field.as_u64().unwrap_or(0) as u32;
            // `prompt` includes the cached tokens.
            let cached = n(&t["cached"]);
            reply.input_tokens += n(&t["prompt"]).saturating_sub(cached);
            reply.cache_read_tokens += cached;
            reply.output_tokens += n(&t["candidates"]);
            if let Some(thoughts) = t["thoughts"].as_u64() {
                *reasoning.get_or_insert(0) += thoughts as u32;
            }
        }
    }
    reply.reasoning_tokens = reasoning;
    Ok(reply)
}
