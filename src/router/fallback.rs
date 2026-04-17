use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

/// Wraps the configured `fallback_chains` map and provides ordered fallback lookup.
/// DB-sourced chains are merged at runtime; DB chains take priority over config chains.
pub struct FallbackChain {
    config_chains: HashMap<String, Vec<String>>,
    db_chains: Arc<ArcSwap<HashMap<String, Vec<String>>>>,
}

impl FallbackChain {
    pub fn new(config_chains: HashMap<String, Vec<String>>) -> Self {
        Self {
            config_chains,
            db_chains: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        }
    }

    /// Replace the live DB failover map (called after DB failover writes).
    pub fn update_db_chains(&self, chains: HashMap<String, Vec<String>>) {
        self.db_chains.store(Arc::new(chains));
    }

    /// Returns the next model to try after `failed_model`.
    /// - If `failed_model` is a chain key, returns the first alternative.
    /// - If `failed_model` appears in a chain's values, returns the next entry.
    /// - Returns `None` if not found or at end of chain.
    pub fn next_after(&self, failed_model: &str) -> Option<String> {
        let db = self.db_chains.load();

        // Check DB chains first
        for (primary, models) in db.iter() {
            if primary == failed_model {
                return models.first().cloned();
            }
            if let Some(pos) = models.iter().position(|m| m == failed_model) {
                return models.get(pos + 1).cloned();
            }
        }

        // Fall back to config chains
        if let Some(alternatives) = self.config_chains.get(failed_model) {
            return alternatives.first().cloned();
        }
        for models in self.config_chains.values() {
            if let Some(pos) = models.iter().position(|m| m == failed_model) {
                return models.get(pos + 1).cloned();
            }
        }
        None
    }

    /// Return all effective chains (DB merged over config), used for display.
    pub fn all_chains(&self) -> HashMap<String, Vec<String>> {
        let mut merged = self.config_chains.clone();
        let db = self.db_chains.load();
        for (k, v) in db.iter() {
            merged.insert(k.clone(), v.clone());
        }
        merged
    }
}
