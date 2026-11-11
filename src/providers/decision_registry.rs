//! Decision adapters by provider name, built lazily from the provider table
//! the way [`crate::providers::embed_registry::EmbeddingRegistry`] builds
//! embedding adapters. Only providers with a decisions surface get an arm;
//! every other provider is a clear "does not serve decisions", never a
//! request sent to a chat endpoint that happens to share the wire shape.

use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::schema::ProviderConfig;
use crate::providers::decision::DecisionAdapter;

pub struct DecisionRegistry {
    adapters: DashMap<String, Arc<dyn DecisionAdapter>>,
    configs: HashMap<String, ProviderConfig>,
}

impl DecisionRegistry {
    pub fn new(configs: HashMap<String, ProviderConfig>) -> Self {
        Self { adapters: DashMap::new(), configs }
    }

    /// The adapter for `provider_name`, built on first use from its config.
    pub fn get(&self, provider_name: &str) -> anyhow::Result<Arc<dyn DecisionAdapter>> {
        if let Some(adapter) = self.adapters.get(provider_name) {
            return Ok(adapter.clone());
        }
        let adapter: Arc<dyn DecisionAdapter> = match provider_name {
            #[cfg(feature = "foundry")]
            "foundry" => {
                let config = self.configs.get(provider_name).ok_or_else(|| {
                    anyhow::anyhow!("provider \"foundry\" is not configured ([providers.foundry] is missing)")
                })?;
                Arc::new(crate::providers::foundry::FoundryDecisionAdapter::new(config)?)
            }
            #[cfg(not(feature = "foundry"))]
            "foundry" => anyhow::bail!(
                "provider \"foundry\" is configured, but this binary was built without the `foundry` \
                 cargo feature — rebuild with `cargo build --release --features foundry`"
            ),
            other => anyhow::bail!("provider \"{other}\" does not serve decisions"),
        };
        let entry = self.adapters.entry(provider_name.to_string()).or_insert(adapter);
        Ok(entry.clone())
    }

    /// Register an adapter under an explicit provider name (tests pin a
    /// fake-upstream adapter here; production adapters are built in [`Self::get`]).
    pub fn register<A: DecisionAdapter + 'static>(&self, provider: &str, adapter: A) {
        self.adapters.insert(provider.to_string(), Arc::new(adapter));
    }

    /// Provider names with a built adapter, for the deep health report.
    pub fn built(&self) -> Vec<(String, Arc<dyn DecisionAdapter>)> {
        self.adapters.iter().map(|e| (e.key().clone(), e.value().clone())).collect()
    }

    /// The provider whose `decision_deployments` (or `systemone_deployments`,
    /// the decision deployments served on the resource's System One route)
    /// names `deployment`, so a bare
    /// decision model (no alias, no `provider/` prefix) resolves to the
    /// deployment the caller asked for instead of falling through to
    /// `routing.default_model`. A decision-only deployment is typically
    /// absent from the chat catalog, which is why the chat resolution path
    /// cannot find it. Providers are tried in name order, so two listing the
    /// same name resolve the same way every time.
    pub fn provider_for_deployment(&self, deployment: &str) -> Option<String> {
        let wanted = deployment.trim();
        if wanted.is_empty() {
            return None;
        }
        let mut names: Vec<&String> = self.configs.keys().collect();
        names.sort();
        names
            .into_iter()
            .find(|name| {
                let config = &self.configs[*name];
                config
                    .decision_deployments
                    .iter()
                    .chain(config.systemone_deployments.iter())
                    .any(|d| d.trim() == wanted)
            })
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_without_a_decisions_surface_is_named_in_the_error() {
        let registry = DecisionRegistry::new(HashMap::new());
        let err = registry.get("openai").err().expect("refused").to_string();
        assert!(err.contains("\"openai\" does not serve decisions"), "{err}");
    }

    #[test]
    fn a_bare_deployment_resolves_to_the_provider_that_lists_it_as_a_decision_deployment() {
        let mut configs = HashMap::new();
        configs.insert(
            "foundry".to_string(),
            ProviderConfig { decision_deployments: vec!["decision-only".into(), " padded ".into()], ..Default::default() },
        );
        configs.insert("openai".to_string(), ProviderConfig::default());
        let registry = DecisionRegistry::new(configs);
        assert_eq!(registry.provider_for_deployment("decision-only").as_deref(), Some("foundry"));
        assert_eq!(registry.provider_for_deployment("padded").as_deref(), Some("foundry"));
        assert_eq!(registry.provider_for_deployment("gpt-4o-mini"), None);
        assert_eq!(registry.provider_for_deployment(""), None);
    }

    #[test]
    fn a_deployment_listed_for_the_systemone_route_resolves_like_any_decision_deployment() {
        let mut configs = HashMap::new();
        configs.insert(
            "foundry".to_string(),
            ProviderConfig { systemone_deployments: vec!["route-two".into()], ..Default::default() },
        );
        let registry = DecisionRegistry::new(configs);
        assert_eq!(registry.provider_for_deployment("route-two").as_deref(), Some("foundry"));
    }

    #[test]
    fn two_providers_listing_one_deployment_resolve_in_name_order() {
        let mut configs = HashMap::new();
        for name in ["zeta", "alpha"] {
            configs.insert(name.to_string(), ProviderConfig { decision_deployments: vec!["shared".into()], ..Default::default() });
        }
        let registry = DecisionRegistry::new(configs);
        assert_eq!(registry.provider_for_deployment("shared").as_deref(), Some("alpha"));
    }

    #[cfg(feature = "foundry")]
    #[test]
    fn an_unconfigured_foundry_names_the_missing_table() {
        let registry = DecisionRegistry::new(HashMap::new());
        let err = registry.get("foundry").err().expect("refused").to_string();
        assert!(err.contains("[providers.foundry]"), "{err}");
    }
}
