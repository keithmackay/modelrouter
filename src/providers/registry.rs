use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::schema::ProviderConfig;
use crate::providers::adapter::ProviderAdapter;

pub struct ProviderRegistry {
    adapters: DashMap<String, Arc<dyn ProviderAdapter>>,
    configs: HashMap<String, ProviderConfig>,
}

impl ProviderRegistry {
    pub fn new(configs: HashMap<String, ProviderConfig>) -> Self {
        Self {
            adapters: DashMap::new(),
            configs,
        }
    }

    pub fn get(&self, provider_name: &str) -> anyhow::Result<Arc<dyn ProviderAdapter>> {
        if let Some(adapter) = self.adapters.get(provider_name) {
            return Ok(adapter.clone());
        }

        // Fall back to first available adapter (useful in tests)
        if self.configs.is_empty() {
            if let Some(entry) = self.adapters.iter().next() {
                return Ok(entry.value().clone());
            }
        }

        let config = self
            .configs
            .get(provider_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown provider: {}", provider_name))?;

        // Feature-gated provider names must never reach the OpenAI-compat
        // fallthrough below: a "vertex" binary built without `--features
        // vertex` would otherwise send every request to api.openai.com with
        // whatever key this config has — a silent misroute that presents as a
        // provider-side 401 (issue #24). serve validates this at startup via
        // `validate_provider_features`; this guard covers every other entry
        // point (CLI subcommands, tests, future callers).
        #[cfg(not(feature = "vertex"))]
        if provider_name == "vertex" {
            anyhow::bail!(
                "provider \"vertex\" is configured, but this binary was built without the `vertex` \
                 cargo feature — rebuild with `cargo build --release --features vertex`"
            );
        }
        #[cfg(not(feature = "bedrock"))]
        if provider_name == "bedrock" {
            anyhow::bail!(
                "provider \"bedrock\" is configured, but this binary was built without the `bedrock` \
                 cargo feature — rebuild with `cargo build --release --features bedrock`"
            );
        }

        let adapter: Arc<dyn ProviderAdapter> = if provider_name == "anthropic" {
            Arc::new(crate::providers::anthropic::AnthropicAdapter::new(config))
        } else if provider_name == "azure" {
            Arc::new(crate::providers::azure_openai::AzureOpenAIAdapter::new(config))
        } else {
            #[cfg(feature = "vertex")]
            if provider_name == "vertex" {
                let adapter = crate::providers::vertex::VertexAdapter::new(config)?;
                let entry = self
                    .adapters
                    .entry(provider_name.to_string())
                    .or_insert(Arc::new(adapter));
                return Ok(entry.clone());
            }
            #[cfg(feature = "bedrock")]
            if provider_name == "bedrock" {
                let bedrock = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(crate::providers::bedrock::BedrockAdapter::new(config))
                });
                // Use or_insert so concurrent callers don't create duplicate adapters
                let entry = self
                    .adapters
                    .entry(provider_name.to_string())
                    .or_insert(Arc::new(bedrock));
                return Ok(entry.clone());
            }
            Arc::new(crate::providers::openai_compat::OpenAICompatAdapter::new(config))
        };

        // Use entry API to prevent duplicate creation under concurrency — only first caller wins
        let entry = self
            .adapters
            .entry(provider_name.to_string())
            .or_insert(adapter);
        Ok(entry.clone())
    }

    /// Test helper: create registry with a single mock adapter for any provider.
    /// When `get` is called and configs are empty, falls back to the first available adapter.
    pub fn new_with_mock<A: ProviderAdapter + 'static>(mock: A) -> Self {
        let registry = Self {
            adapters: DashMap::new(),
            configs: HashMap::new(),
        };
        let mock_arc: Arc<dyn ProviderAdapter> = Arc::new(mock);
        registry.adapters.insert("__mock__".to_string(), mock_arc);
        registry
    }
}
