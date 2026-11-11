//! `codex exec --json`: Codex's non-interactive mode, a JSONL event stream.

use serde_json::Value;

use super::{Invocation, Reply};

pub(super) fn args(inv: &Invocation) -> Vec<String> {
    let mut args: Vec<String> = vec!["exec".into()];
    if let Some(id) = &inv.resume {
        args.extend(["resume".into(), "--json".into(), "--skip-git-repo-check".into()]);
        args.extend(["-m".into(), inv.model.clone(), id.clone(), "-".into()]);
    } else {
        // Read-only sandbox in an empty working directory: Codex is an agent,
        // and nothing it decides to run should change anything.
        args.extend(["--json".into(), "--skip-git-repo-check".into()]);
        args.extend(["--sandbox".into(), "read-only".into()]);
        args.extend(["-m".into(), inv.model.clone(), "-".into()]);
    }
    args
}

/// `codex exec` has no system-prompt flag, so a new session carries the
/// caller's system prompt at the top of its first message. A resumed session
/// already has it.
pub(super) fn stdin(inv: &Invocation) -> String {
    match (&inv.system, &inv.resume) {
        (Some(system), None) => format!("<system>\n{system}\n</system>\n\n{}", inv.prompt),
        _ => inv.prompt.clone(),
    }
}

pub(super) fn parse(stdout: &str) -> anyhow::Result<Reply> {
    let mut reply = Reply::default();
    let mut finished = false;
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str().unwrap_or_default() {
            "thread.started" => reply.session_id = event["thread_id"].as_str().map(String::from),
            "item.completed" if event["item"]["type"] == "agent_message" => {
                if let Some(text) = event["item"]["text"].as_str() {
                    reply.text = text.to_string();
                }
            }
            "turn.completed" => {
                finished = true;
                let u = &event["usage"];
                let n = |field: &Value| field.as_u64().unwrap_or(0) as u32;
                // `input_tokens` includes the cached ones.
                let cached = n(&u["cached_input_tokens"]);
                reply.input_tokens = n(&u["input_tokens"]).saturating_sub(cached);
                reply.cache_read_tokens = cached;
                reply.cache_write_tokens = n(&u["cache_write_input_tokens"]);
                reply.output_tokens = n(&u["output_tokens"]);
                reply.reasoning_tokens = u["reasoning_output_tokens"].as_u64().map(|t| t as u32);
            }
            "turn.failed" => {
                return Err(super::vendor_error(format!(
                    "codex reported an error: {}",
                    unwrap_message(&event["error"]["message"])
                )))
            }
            "error" => {
                return Err(super::vendor_error(format!(
                    "codex reported an error: {}",
                    unwrap_message(&event["message"])
                )))
            }
            _ => {}
        }
    }
    if !finished {
        anyhow::bail!("codex ended without completing the turn: {}", super::truncate(stdout, 500));
    }
    Ok(reply)
}

/// Codex relays an upstream API error as that error's JSON in a string;
/// report the inner `error.message` when there is one.
fn unwrap_message(message: &Value) -> String {
    let raw = message.as_str().unwrap_or("no detail");
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|inner| inner["error"]["message"].as_str().map(String::from))
        .unwrap_or_else(|| raw.to_string())
}
