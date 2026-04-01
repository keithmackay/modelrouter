use std::sync::Arc;

use crate::api::app::DatabaseProvider;

pub async fn check_permission(
    db: &Arc<dyn DatabaseProvider>,
    hook_name: &str,
    capability: &str,
) -> anyhow::Result<bool> {
    db.has_permission(hook_name, capability).await
}

/// Sync hook capabilities from config into the DB at startup.
/// Adds permissions declared in config if they don't already exist.
/// DOES NOT remove permissions — removal is operator-controlled via API.
pub async fn sync_hook_permissions(
    db: &Arc<dyn DatabaseProvider>,
    hooks: &crate::config::schema::HooksConfig,
) -> anyhow::Result<()> {
    for hook in &hooks.pipeline {
        for cap in &hook.capabilities {
            if !db.has_permission(&hook.name, cap).await? {
                db.grant_permission(&hook.name, cap, None).await?;
            }
        }
    }
    Ok(())
}
