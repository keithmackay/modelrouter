//! The request path's one way to write a cost-ledger row: store it, then emit
//! the `usage` callback event built from the stored row. A failed write emits
//! nothing, so every event names a row that exists.

use crate::api::app::DatabaseProvider;
use crate::callbacks::CallbackDispatcher;
use crate::db::models::{CostLedgerEntry, NewCostLedgerEntry};
use crate::db::repositories::costs::CostRepository;

/// Record a billed call and emit its `usage` event.
pub async fn record_usage(
    db: &dyn DatabaseProvider,
    callbacks: &CallbackDispatcher,
    entry: NewCostLedgerEntry,
) -> anyhow::Result<CostLedgerEntry> {
    let row = CostRepository::create(db, entry).await?;
    callbacks.dispatch_usage(&row);
    Ok(row)
}

/// Record a response-cache hit and emit its `usage` event. `entry.cost_usd`
/// is the avoided cost; it is stored as `saved_usd` (see
/// [`CostRepository::create_cache_hit`]).
pub async fn record_cache_hit_usage(
    db: &dyn DatabaseProvider,
    callbacks: &CallbackDispatcher,
    entry: NewCostLedgerEntry,
) -> anyhow::Result<CostLedgerEntry> {
    let row = CostRepository::create_cache_hit(db, entry).await?;
    callbacks.dispatch_usage(&row);
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callbacks::{CallbackBackend, CallbackDispatcher, CallbackEvent};
    use crate::db::models::NewCostLedgerEntry;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    /// Keeps every usage payload it is handed.
    struct Recorder(Arc<Mutex<Vec<Value>>>);

    impl CallbackBackend for Recorder {
        fn send(&self, _event: CallbackEvent) {}

        fn send_usage(&self, payload: &Value) {
            self.0.lock().unwrap().push(payload.clone());
        }
    }

    async fn migrated_db() -> crate::db::sqlite::SqliteDb {
        let db = crate::db::sqlite::SqliteDb::connect(":memory:").await.unwrap();
        crate::db::migrations::run_migrations(&db.pool).await.unwrap();
        db
    }

    fn recording_dispatcher() -> (CallbackDispatcher, Arc<Mutex<Vec<Value>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (CallbackDispatcher::new(vec![Box::new(Recorder(seen.clone()))]), seen)
    }

    fn entry(user_id: i64) -> NewCostLedgerEntry {
        NewCostLedgerEntry {
            user_id,
            prompt_id: None,
            model: "m".into(),
            provider: "p".into(),
            project: Some("proj".into()),
            tokens_in: 10,
            tokens_out: 5,
            cost_usd: 0.25,
            api_key_id: None,
            attribution_correlation_id: Some("run-1".into()),
            attribution_tags: r#"{"k":"v"}"#.into(),
            experiment_id: None,
            experiment_variant: None,
            tokens_estimated: false,
            call_kind: crate::db::models::CallKind::Chat,
            cache_read_tokens: 3,
            cache_write_tokens: 0,
        }
    }

    async fn user(db: &crate::db::sqlite::SqliteDb) -> i64 {
        use crate::db::repositories::users::UserRepository;
        UserRepository::create(db, crate::db::models::NewUser { name: "alice".into(), email: None })
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn each_recorded_row_emits_one_usage_event_built_from_the_stored_row() {
        let db = migrated_db().await;
        let uid = user(&db).await;
        let (callbacks, seen) = recording_dispatcher();

        let first = record_usage(&db, &callbacks, entry(uid)).await.unwrap();
        let second = record_usage(&db, &callbacks, entry(uid)).await.unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], crate::callbacks::usage::from_ledger(&first));
        assert_eq!(seen[1]["ledger_id"], second.id);
        assert_eq!(seen[0]["occurred_at"], first.created_at.as_str());
    }

    #[tokio::test]
    async fn a_cache_hit_row_is_stored_as_a_hit_and_emitted_as_one() {
        let db = migrated_db().await;
        let uid = user(&db).await;
        let (callbacks, seen) = recording_dispatcher();

        let row = record_cache_hit_usage(&db, &callbacks, entry(uid)).await.unwrap();
        assert!(row.cache_hit);

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["cache_hit"], true);
        assert_eq!(seen[0]["cost_usd"], 0.0);
        assert_eq!(seen[0]["saved_usd"], 0.25);
    }

    #[tokio::test]
    async fn a_failed_write_emits_nothing() {
        // No migrations: the cost_ledger table does not exist.
        let db = crate::db::sqlite::SqliteDb::connect(":memory:").await.unwrap();
        let (callbacks, seen) = recording_dispatcher();

        assert!(record_usage(&db, &callbacks, entry(1)).await.is_err());
        assert!(record_cache_hit_usage(&db, &callbacks, entry(1)).await.is_err());
        assert!(seen.lock().unwrap().is_empty());
    }
}
