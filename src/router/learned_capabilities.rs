//! Model capabilities learned from provider rejections.
//!
//! When a provider rejects a request because it carried `temperature`, the
//! router retries that request once without the parameter and records the
//! rejection here, keyed by the exact routed model id (provider segments
//! stripped, lowercased, any `@version` kept — one snapshot's rejection does
//! not mark its whole family). From then on the parameter is never sent to
//! that model. Entries persist in the `learned_model_capabilities` table, are
//! loaded at startup, and are listed and cleared through the admin API.
//!
//! Precedence: a config `[[model_capabilities]]` entry beats a learned entry,
//! which beats the built-in table (see
//! [`crate::router::model_capabilities::temperature_allowed`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::Serialize;

use crate::db::models::LearnedModelCapability;

/// One learned entry plus how many requests have had the parameter removed
/// because of it since this process started.
#[derive(Debug)]
struct Entry {
    row: LearnedModelCapability,
    stripped: AtomicU64,
}

/// Admin view of one learned entry.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LearnedCapabilityView {
    #[serde(flatten)]
    pub row: LearnedModelCapability,
    /// Requests sent without `temperature` because of this entry, since start.
    pub stripped_since_start: u64,
}

#[derive(Debug, Default)]
pub struct LearnedCapabilities {
    entries: RwLock<HashMap<String, Arc<Entry>>>,
}

/// The key a model is learned under: every leading provider segment
/// stripped, lowercased, `@version` kept.
pub fn learned_key(model: &str) -> String {
    let bare = match model.rfind('/') {
        Some(pos) => &model[pos + 1..],
        None => model,
    };
    bare.to_lowercase()
}

impl LearnedCapabilities {
    /// Replace every entry with `rows` (startup load).
    pub fn replace_all(&self, rows: Vec<LearnedModelCapability>) {
        let map = rows
            .into_iter()
            .map(|row| (row.model.clone(), Arc::new(Entry { row, stripped: AtomicU64::new(0) })))
            .collect();
        match self.entries.write() {
            Ok(mut guard) => *guard = map,
            Err(poisoned) => *poisoned.into_inner() = map,
        }
    }

    fn get(&self, model: &str) -> Option<Arc<Entry>> {
        let key = learned_key(model);
        match self.entries.read() {
            Ok(guard) => guard.get(&key).cloned(),
            Err(poisoned) => poisoned.into_inner().get(&key).cloned(),
        }
    }

    /// The learned temperature capability for this exact model, if any.
    pub fn temperature(&self, model: &str) -> Option<bool> {
        self.get(model).map(|e| e.row.supports_temperature)
    }

    /// Count one request sent without `temperature` because of the entry.
    pub fn note_stripped(&self, model: &str) {
        if let Some(entry) = self.get(model) {
            entry.stripped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record that `model` rejects `temperature`. Returns the row to persist
    /// when this is new, `None` when it was already known.
    pub fn learn_temperature_rejected(&self, model: &str, error: &str) -> Option<LearnedModelCapability> {
        let key = learned_key(model);
        let row = LearnedModelCapability {
            model: key.clone(),
            supports_temperature: false,
            error: error.chars().take(1000).collect(),
            learned_at: chrono::Utc::now().to_rfc3339(),
        };
        let mut guard = match self.entries.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.get(&key).is_some_and(|e| !e.row.supports_temperature) {
            return None;
        }
        guard.insert(key, Arc::new(Entry { row: row.clone(), stripped: AtomicU64::new(0) }));
        Some(row)
    }

    /// Forget a learned entry. Returns whether one existed.
    pub fn remove(&self, model: &str) -> bool {
        let key = learned_key(model);
        match self.entries.write() {
            Ok(mut guard) => guard.remove(&key).is_some(),
            Err(poisoned) => poisoned.into_inner().remove(&key).is_some(),
        }
    }

    /// Every entry, sorted by model, for the admin API.
    pub fn snapshot(&self) -> Vec<LearnedCapabilityView> {
        let guard = match self.entries.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut views: Vec<LearnedCapabilityView> = guard
            .values()
            .map(|e| LearnedCapabilityView {
                row: e.row.clone(),
                stripped_since_start: e.stripped.load(Ordering::Relaxed),
            })
            .collect();
        views.sort_by(|a, b| a.row.model.cmp(&b.row.model));
        views
    }
}

/// Learn from a provider rejecting `temperature` for `model`: update the live
/// set, persist the row, and log once. A model already known is not re-logged.
/// A failed write is logged and the entry still applies until restart.
pub async fn record_temperature_rejection<R>(
    learned: &LearnedCapabilities,
    store: &R,
    model: &str,
    err: &anyhow::Error,
) where
    R: crate::db::repositories::LearnedCapabilityRepository + ?Sized,
{
    let Some(row) = learned.learn_temperature_rejected(model, &err.to_string()) else {
        return;
    };
    tracing::warn!(
        model,
        learned_key = row.model.as_str(),
        error = %err,
        "provider rejected `temperature` for this model; retried without it and will \
         never send it to this model again (stored as a learned model capability; \
         clear it via DELETE /admin/api/model-capabilities/learned/<model> if the \
         provider restores support)"
    );
    if let Err(e) = store.upsert_learned_capability(&row).await {
        tracing::error!(model, error = %e, "failed to persist a learned model capability");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learning_is_per_exact_model_and_version() {
        let learned = LearnedCapabilities::default();
        let row = learned
            .learn_temperature_rejected("vertex/acme/model-x@v1", "rejected")
            .expect("new entry");
        assert_eq!(row.model, "model-x@v1");
        assert_eq!(learned.temperature("acme/Model-X@v1"), Some(false));
        assert_eq!(learned.temperature("model-x@v2"), None);
        assert_eq!(learned.temperature("model-x"), None);
        // Learning it again is not new.
        assert!(learned.learn_temperature_rejected("model-x@v1", "again").is_none());
    }

    #[test]
    fn stripped_count_and_removal_show_in_the_snapshot() {
        let learned = LearnedCapabilities::default();
        learned.learn_temperature_rejected("model-y", "rejected");
        learned.note_stripped("vertex/acme/model-y");
        learned.note_stripped("model-y");
        let views = learned.snapshot();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].stripped_since_start, 2);
        assert!(learned.remove("model-y"));
        assert!(learned.snapshot().is_empty());
        assert_eq!(learned.temperature("model-y"), None);
    }
}
