use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use crate::config::schema::ProviderConfig;
use crate::providers::embedding::EmbeddingAdapter;

pub struct EmbeddingRegistry {
    adapters: DashMap<String, Arc<dyn EmbeddingAdapter>>,
    configs: HashMap<String, ProviderConfig>,
}

impl EmbeddingRegistry {
    pub fn new(configs: HashMap<String, ProviderConfig>) -> Self {
        Self {
            adapters: DashMap::new(),
            configs,
        }
    }

    pub fn get(&self, provider_name: &str) -> anyhow::Result<Arc<dyn EmbeddingAdapter>> {
        if let Some(adapter) = self.adapters.get(provider_name) {
            return Ok(adapter.clone());
        }

        // Fall back to first available adapter (test-only path when configs is empty)
        if self.configs.is_empty() {
            if let Some(entry) = self.adapters.iter().next() {
                return Ok(entry.value().clone());
            }
        }

        let config = self
            .configs
            .get(provider_name)
            .ok_or_else(|| anyhow::anyhow!("No embedding adapter for provider: {}", provider_name))?;

        let adapter: Arc<dyn EmbeddingAdapter> = Arc::new(
            crate::providers::openai_embed::OpenAIEmbeddingAdapter::new(config),
        );

        let entry = self
            .adapters
            .entry(provider_name.to_string())
            .or_insert(adapter);
        Ok(entry.clone())
    }

    /// Test helper: create registry with a single mock adapter for any provider.
    pub fn new_with_mock<A: EmbeddingAdapter + 'static>(mock: A) -> Self {
        let registry = Self {
            adapters: DashMap::new(),
            configs: HashMap::new(),
        };
        let mock_arc: Arc<dyn EmbeddingAdapter> = Arc::new(mock);
        registry.adapters.insert("__mock__".to_string(), mock_arc);
        registry
    }
}
