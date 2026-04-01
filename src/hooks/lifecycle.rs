use std::time::Duration;
use crate::config::schema::LifecycleHookConfig;

pub fn fire(hook: &LifecycleHookConfig, payload: serde_json::Value) -> tokio::task::JoinHandle<()> {
    let hook = hook.clone();
    tokio::spawn(async move {
        let result = tokio::time::timeout(
            Duration::from_secs(hook.timeout_secs),
            run_subprocess(&hook.exec, &payload),
        )
        .await;
        match result {
            Err(_timeout) => tracing::warn!(hook = %hook.name, "lifecycle hook timed out"),
            Ok(Err(e)) => tracing::error!(hook = %hook.name, error = %e, "lifecycle hook failed"),
            Ok(Ok(status)) if !status.success() => {
                tracing::warn!(hook = %hook.name, "lifecycle hook exited non-zero")
            }
            Ok(Ok(_)) => tracing::debug!(hook = %hook.name, "lifecycle hook completed"),
        }
    })
}

async fn run_subprocess(
    exec: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<std::process::ExitStatus> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let mut child = Command::new(exec)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(serde_json::to_string(payload)?.as_bytes())
            .await?;
        // stdin dropped here, sends EOF
    }

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            tracing::debug!("lifecycle hook stderr: {}", stderr);
        }
    }
    Ok(output.status)
}

pub fn request_received_payload(
    user_name: &str,
    model: &str,
    message_count: usize,
) -> serde_json::Value {
    serde_json::json!({
        "event": "on_request_received",
        "user_name": user_name,
        "model": model,
        "message_count": message_count,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    })
}

pub fn response_sent_payload(
    user_name: &str,
    model: &str,
    routed_model: &str,
    cost_usd: f64,
    latency_ms: i64,
) -> serde_json::Value {
    serde_json::json!({
        "event": "on_response_sent",
        "user_name": user_name,
        "model": model,
        "routed_model": routed_model,
        "cost_usd": cost_usd,
        "latency_ms": latency_ms,
    })
}

pub fn budget_exceeded_payload(
    user_name: &str,
    model: &str,
    limit_usd: f64,
    spent_usd: f64,
    window: &str,
) -> serde_json::Value {
    serde_json::json!({
        "event": "on_budget_exceeded",
        "user_name": user_name,
        "model": model,
        "limit_usd": limit_usd,
        "spent_usd": spent_usd,
        "window": window,
    })
}

pub fn stream_complete_payload(
    user_name: &str,
    model: &str,
    approx_tokens: u32,
    cost_usd: f64,
) -> serde_json::Value {
    serde_json::json!({
        "event": "on_stream_complete",
        "user_name": user_name,
        "model": model,
        "approx_tokens": approx_tokens,
        "cost_usd": cost_usd,
    })
}

pub fn error_payload(
    user_name: &str,
    model: &str,
    error_type: &str,
    message: &str,
) -> serde_json::Value {
    serde_json::json!({
        "event": "on_error",
        "user_name": user_name,
        "model": model,
        "error_type": error_type,
        "message": message,
    })
}

pub fn user_disabled_payload(user_name: &str, disabled_by: &str) -> serde_json::Value {
    serde_json::json!({
        "event": "on_user_disabled",
        "user_name": user_name,
        "disabled_by": disabled_by,
    })
}
