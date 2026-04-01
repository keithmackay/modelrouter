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
/// Logs a warning for any declared capability that has no operator grant in hook_permissions.
/// Does NOT auto-insert grants — operators must explicitly add rows with can_run=true.
pub async fn sync_hook_permissions(
    db: &Arc<dyn DatabaseProvider>,
    hooks: &crate::config::schema::HooksConfig,
) -> anyhow::Result<()> {
    for hook in &hooks.pipeline {
        for cap in &hook.capabilities {
            if !db.has_permission(&hook.name, cap).await? {
                tracing::warn!(
                    hook = %hook.name,
                    capability = %cap,
                    "Hook declares capability '{}' but no operator grant exists in hook_permissions \
                     table. Capability is inactive until an operator inserts a row with can_run=true.",
                    cap
                );
            }
        }
    }
    Ok(())
}
