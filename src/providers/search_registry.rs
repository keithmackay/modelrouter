use crate::config::schema::ProviderConfig;
use crate::providers::search::SearchAdapter;
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;

/// Search engines supported by the registry's default construction path.
/// Extend this (and the `match` in `get`) to add a new engine.
#[cfg(not(feature = "vertex"))]
const SUPPORTED_ENGINES: &[&str] = &["tavily"];
/// `vertex` is only listed when the feature is compiled in — otherwise
/// `api/routes/search.rs` would accept the engine at the gate and then fail in
/// the registry, reporting a configuration problem for what is a build problem.
#[cfg(feature = "vertex")]
const SUPPORTED_ENGINES: &[&str] = &["tavily", "vertex"];

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

    /// Engines this registry can actually serve, deduplicated and sorted for a
    /// stable error message. Used to resolve the engine when a request omits
    /// one: "the single available search provider" is a defensible default in a
    /// way that a hardcoded provider name is not.
    ///
    /// An engine counts if it has a config we could build an adapter from OR an
    /// adapter already registered — `new_with_mock` populates only the latter,
    /// and an engine that will serve a request is available whether or not the
    /// operator's config.toml is the reason it exists.
    pub fn configured_engines(&self) -> Vec<String> {
        let mut engines: Vec<String> = self
            .configs
            .keys()
            .cloned()
            .chain(self.adapters.iter().map(|e| e.key().clone()))
            .filter(|e| is_supported_engine(e))
            .collect();
        engines.sort();
        engines.dedup();
        engines
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
            #[cfg(feature = "vertex")]
            "vertex" => Arc::new(crate::providers::vertex::VertexSearchAdapter::new(config)?),
            other => anyhow::bail!("Unsupported search engine: {}", other),
        };

        // Use entry API to prevent duplicate creation under concurrency — only first caller wins
        let entry = self.adapters.entry(engine.to_string()).or_insert(adapter);
        Ok(entry.clone())
    }

    /// Test helper: create registry with a single mock adapter registered as "tavily".
    pub fn new_with_mock<A: SearchAdapter + 'static>(mock: A) -> Self {
        Self::new_with_mock_engines(vec![("tavily", Arc::new(mock))])
    }

    /// Test helper: register mock adapters under arbitrary engine names, so a
    /// test can exercise a host that has something other than — or more than —
    /// Tavily available.
    pub fn new_with_mock_engines(mocks: Vec<(&str, Arc<dyn SearchAdapter>)>) -> Self {
        let registry = Self {
            adapters: DashMap::new(),
            configs: HashMap::new(),
        };
        for (engine, adapter) in mocks {
            registry.adapters.insert(engine.to_string(), adapter);
        }
        registry
    }
}
