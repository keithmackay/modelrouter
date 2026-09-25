use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::schema::{ProviderConfig, TierTimeoutsConfig};
use crate::providers::adapter::ProviderAdapter;

pub struct ProviderRegistry {
    adapters: DashMap<String, Arc<dyn ProviderAdapter>>,
    configs: HashMap<String, ProviderConfig>,
    tier_timeouts: TierTimeoutsConfig,
}

impl ProviderRegistry {
    pub fn new(configs: HashMap<String, ProviderConfig>) -> Self {
        Self::new_with_tier_timeouts(configs, TierTimeoutsConfig::default())
    }

    pub fn new_with_tier_timeouts(
        configs: HashMap<String, ProviderConfig>,
        tier_timeouts: TierTimeoutsConfig,
    ) -> Self {
        Self {
            adapters: DashMap::new(),
            configs,
            tier_timeouts,
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
        #[cfg(not(feature = "foundry"))]
        if provider_name == "foundry" {
            anyhow::bail!(
                "provider \"foundry\" is configured, but this binary was built without the `foundry` \
                 cargo feature — rebuild with `cargo build --release --features foundry`"
            );
        }

        let adapter: Arc<dyn ProviderAdapter> = if provider_name == "anthropic" {
            Arc::new(crate::providers::anthropic::AnthropicAdapter::new(
                config,
                self.tier_timeouts.clone(),
            ))
        } else if provider_name == "azure" {
            Arc::new(crate::providers::azure_openai::AzureOpenAIAdapter::new(
                config,
                self.tier_timeouts.clone(),
            ))
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
            #[cfg(feature = "foundry")]
            if provider_name == "foundry" {
                let adapter = crate::providers::foundry::FoundryAdapter::new(config)?;
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
            // A provider section configured only to back a dedicated route
            // (e.g. `[providers.typesafe]` for `/v1/systemone`) must not also
            // become a generic chat provider just by existing in config — see
            // #97. Routes for those providers look the config up directly via
            // `state.settings.providers.get(name)`, which is unaffected by
            // this registry gate.
            if !config.generic_chat {
                anyhow::bail!(
                    "Unknown provider: {} (configured, but not enabled for /v1/chat/completions)",
                    provider_name
                );
            }
            Arc::new(crate::providers::openai_compat::OpenAICompatAdapter::new(
                config,
                self.tier_timeouts.clone(),
            ))
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
            tier_timeouts: TierTimeoutsConfig::default(),
        };
        let mock_arc: Arc<dyn ProviderAdapter> = Arc::new(mock);
        registry.adapters.insert("__mock__".to_string(), mock_arc);
        registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(generic_chat: bool) -> ProviderConfig {
        ProviderConfig {
            api_key: "secret".to_string(),
            generic_chat,
            ..Default::default()
        }
    }

    #[test]
    fn provider_with_generic_chat_false_is_not_reachable_as_a_chat_provider() {
        let mut configs = HashMap::new();
        configs.insert("typesafe".to_string(), config_with(false));
        let registry = ProviderRegistry::new(configs);

        match registry.get("typesafe") {
            Ok(_) => panic!("expected an unknown-provider error, got an adapter"),
            Err(err) => assert!(
                err.to_string().contains("Unknown provider"),
                "expected an unknown-provider error, got: {err}"
            ),
        }
    }

    #[test]
    fn provider_with_generic_chat_true_falls_back_to_openai_compat() {
        let mut configs = HashMap::new();
        configs.insert("custom".to_string(), config_with(true));
        let registry = ProviderRegistry::new(configs);

        assert!(registry.get("custom").is_ok());
    }

    #[test]
    fn generic_chat_defaults_to_true_for_configs_without_it_set() {
        // Every provider section written before this flag existed (toml/env)
        // must keep behaving as a generic chat provider.
        let config: ProviderConfig = toml::from_str("api_key = \"secret\"\n").unwrap();
        assert!(config.generic_chat);
    }
}
