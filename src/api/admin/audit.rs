use std::sync::Arc;
use crate::api::app::DatabaseProvider;

pub async fn audit(
    db: &Arc<dyn DatabaseProvider>,
    actor_id: Option<i64>,
    actor_name: &str,
    action: &str,
    target: Option<String>,
    before_json: Option<String>,
    after_json: Option<String>,
) {
    use crate::db::{repositories::audit::AuditRepository, models::NewAuditLogEntry};
    let entry = NewAuditLogEntry {
        actor_id,
        actor_name: actor_name.to_string(),
        action: action.to_string(),
        target,
        before_json,
        after_json,
    };
    if let Err(e) = AuditRepository::create(&**db, entry).await {
        tracing::error!("Failed to write audit log: {}", e);
    }
}
