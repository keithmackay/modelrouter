use std::collections::HashMap;

/// Wraps the configured `fallback_chains` map and provides ordered fallback lookup.
pub struct FallbackChain {
    chains: HashMap<String, Vec<String>>,
}

impl FallbackChain {
    pub fn new(chains: HashMap<String, Vec<String>>) -> Self {
        Self { chains }
    }

    /// Returns the next model to try after `failed_model`.
    /// - If `failed_model` is a chain key, returns the first alternative.
    /// - If `failed_model` appears in a chain's values, returns the next entry.
    /// - Returns `None` if not found or at end of chain.
    pub fn next_after(&self, failed_model: &str) -> Option<&str> {
        // Check if failed_model is a primary key (first in chain)
        if let Some(alternatives) = self.chains.get(failed_model) {
            return alternatives.first().map(|s| s.as_str());
        }
        // Check if failed_model appears in any chain's values list
        for models in self.chains.values() {
            if let Some(pos) = models.iter().position(|m| m == failed_model) {
                return models.get(pos + 1).map(|s| s.as_str());
            }
        }
        None
    }
}
