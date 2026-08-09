use crate::config::schema::ProviderConfig;
use crate::providers::search::SearchAdapter;
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;

/// Search engines supported by the registry's default construction path.
/// Extend this (and the `match` in `get`) to add a new engine.
const SUPPORTED_ENGINES: &[&str] = &["tavily"];

pub fn is_supported_engine(engine: &str) -> bool {
    SUPPORTED_ENGINES.contains(&engine)
}

pub struct SearchRegistry {
    adapters: DashMap<String, Arc<dyn SearchAdapter>>,
    configs: HashMap<String, ProviderConfig>,
}

impl SearchRegistry {
    pub fn new(configs: HashMap<String, ProviderConfig>) -> Self {
        Self {
            adapters: DashMap::new(),
            configs,
        }
    }

    pub fn get(&self, engine: &str) -> anyhow::Result<Arc<dyn SearchAdapter>> {
        if let Some(adapter) = self.adapters.get(engine) {
            return Ok(adapter.clone());
        }

        let config = self.configs.get(engine).ok_or_else(|| {
            anyhow::anyhow!("No search adapter configured for engine: {}", engine)
        })?;

        let adapter: Arc<dyn SearchAdapter> = match engine {
            "tavily" => Arc::new(crate::providers::tavily::TavilyAdapter::new(config)),
            other => anyhow::bail!("Unsupported search engine: {}", other),
        };

        // Use entry API to prevent duplicate creation under concurrency — only first caller wins
        let entry = self.adapters.entry(engine.to_string()).or_insert(adapter);
        Ok(entry.clone())
    }

    /// Test helper: create registry with a single mock adapter registered as "tavily".
    pub fn new_with_mock<A: SearchAdapter + 'static>(mock: A) -> Self {
        let registry = Self {
            adapters: DashMap::new(),
            configs: HashMap::new(),
        };
        let mock_arc: Arc<dyn SearchAdapter> = Arc::new(mock);
        registry.adapters.insert("tavily".to_string(), mock_arc);
        registry
    }
}
