use std::{sync::Arc, time::Duration};

use crate::{api::app::DatabaseProvider, config::schema::PipelineHookConfig};

pub async fn run_pre_request(
    hooks: &[PipelineHookConfig],
    db: &Arc<dyn DatabaseProvider>,
    body: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    run_pipeline_hooks(hooks, db, "pre_request", body).await
}

pub async fn run_post_response(
    hooks: &[PipelineHookConfig],
    db: &Arc<dyn DatabaseProvider>,
    body: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    run_pipeline_hooks(hooks, db, "post_response", body).await
}

async fn run_pipeline_hooks(
    hooks: &[PipelineHookConfig],
    db: &Arc<dyn DatabaseProvider>,
    event: &str,
    mut payload: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    for hook in hooks.iter().filter(|h| h.event == event) {
        let start = std::time::Instant::now();
        let result = run_single_pipeline_hook(hook, db, payload.clone()).await;
        let duration_ms = start.elapsed().as_millis() as i64;
        let success = result.is_ok();

        // Record metrics (fire-and-forget)
        let db_clone = db.clone();
        let hook_name = hook.name.clone();
        tokio::spawn(async move {
            use crate::db::models::HookMetric;
            let metric = HookMetric {
                hook_name,
                invoked_at: chrono::Utc::now().to_rfc3339(),
                duration_ms,
                success,
            };
            if let Err(e) = db_clone.record_metric(metric).await {
                tracing::debug!("Failed to record hook metric: {}", e);
            }
        });

        match result {
            Ok(new_payload) => payload = new_payload,
            Err(e) if hook.fail_open => {
                tracing::warn!(hook = %hook.name, error = %e, "pipeline hook failed (fail_open — using original)");
                // payload unchanged — use original
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "pipeline hook '{}' failed: {}",
                    hook.name,
                    e
                ));
            }
        }
    }
    Ok(payload)
}

async fn run_single_pipeline_hook(
    hook: &PipelineHookConfig,
    db: &Arc<dyn DatabaseProvider>,
    payload: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let required_cap = match hook.event.as_str() {
        "pre_request" => "mutate_request",
        "post_response" => "mutate_response",
        _ => return Ok(payload),
    };

    // Check if hook claims this capability
    let claims_capability = hook.capabilities.contains(&required_cap.to_string());

    // If the hook claims a capability, check DB permission.
    // If the hook claims no capabilities, it can still run (read-only observer).
    let can_run = if claims_capability {
        db.has_permission(&hook.name, required_cap).await?
    } else {
        true // No capabilities claimed — always allow to run (read-only)
    };

    if !can_run {
        // Hook claims a capability it doesn't have permission for — skip entirely
        tracing::debug!(hook = %hook.name, "pipeline hook skipped: missing DB permission for '{}'", required_cap);
        return Ok(payload);
    }

    let result = tokio::time::timeout(
        Duration::from_secs(hook.timeout_secs),
        run_subprocess_bidirectional(&hook.exec, &payload),
    )
    .await;

    let can_mutate = claims_capability; // can only mutate if it both claimed AND has permission (already checked above)

    match result {
        Ok(Ok(output)) => {
            if can_mutate {
                match serde_json::from_str::<serde_json::Value>(&output) {
                    Ok(new_payload) => Ok(new_payload),
                    Err(e) => {
                        if hook.fail_open {
                            tracing::warn!(
                                hook = %hook.name,
                                "pipeline hook returned invalid JSON (fail_open)"
                            );
                            Ok(payload)
                        } else {
                            Err(anyhow::anyhow!(
                                "pipeline hook '{}' returned invalid JSON: {}",
                                hook.name,
                                e
                            ))
                        }
                    }
                }
            } else {
                // Read-only hook ran, discard output
                Ok(payload)
            }
        }
        Ok(Err(e)) => {
            if hook.fail_open {
                Ok(payload)
            } else {
                Err(e)
            }
        }
        Err(_timeout) => {
            if hook.fail_open {
                tracing::warn!(hook = %hook.name, "pipeline hook timed out (fail_open)");
                Ok(payload)
            } else {
                Err(anyhow::anyhow!("pipeline hook '{}' timed out", hook.name))
            }
        }
    }
}

async fn run_subprocess_bidirectional(
    exec: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let mut child = Command::new(exec)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let payload_bytes = serde_json::to_vec(payload)?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&payload_bytes).await?;
        // Drop stdin here so the subprocess gets EOF
    }

    let output = child.wait_with_output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("subprocess exited non-zero: {}", stderr));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
