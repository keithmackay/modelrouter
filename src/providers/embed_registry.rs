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

        let config = self
            .configs
            .get(provider_name)
            .ok_or_else(|| anyhow::anyhow!("No embedding adapter for provider: {}", provider_name))?;

        // The OpenAI-compatible adapter stays the DEFAULT arm rather than
        // becoming one of several named arms: Ollama, Azure, LM Studio and
        // anything else speaking that wire shape all reach embeddings through
        // it, and an exhaustive match would turn every one of them into an
        // "unsupported provider" error. Only providers whose embedding API is
        // genuinely different get an arm of their own.
        let adapter: Arc<dyn EmbeddingAdapter> = match provider_name {
            #[cfg(feature = "vertex")]
            "vertex" => Arc::new(
                crate::providers::vertex::VertexEmbeddingAdapter::new(config)?,
            ),
            _ => Arc::new(
                crate::providers::openai_embed::OpenAIEmbeddingAdapter::new(config),
            ),
        };

        // Use entry API to prevent duplicate creation under concurrency — only first caller wins
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
        registry.adapters.insert("openai".to_string(), mock_arc);
        registry
    }
}
