//! Catalog aggregation across configured providers (issue #34).
//!
//! `catalog_for` mirrors `registry.rs`'s adapter dispatch — including its
//! hard-won rule from #24: a feature-gated provider name must NEVER fall
//! through to the OpenAI-compat arm. Providers without a catalog surface
//! return `None`, which the aggregate reports as "not supported" rather than
//! an empty model list. Search-only providers have no model catalog at all:
//! the aggregate leaves them out instead of reporting them.

use crate::config::schema::ProviderConfig;
use crate::providers::catalog::ProviderCatalog;
use futures::future::join_all;
use std::collections::HashMap;
use std::sync::Arc;

/// Catalog capability for one configured provider, or `None` when the
/// provider has no catalog surface in this binary.
pub fn catalog_for(provider_name: &str, config: &ProviderConfig) -> Option<Arc<dyn ProviderCatalog>> {
    if crate::providers::search_registry::is_search_only(provider_name) {
        // Search-only providers have no model catalog; never let them reach
        // the compat arm, which would query a default api_base.
        return None;
    }
    match provider_name {
        // Azure OpenAI: deployments listing needs the management plane (#33).
        "azure" => None,
        // Bedrock has no catalog impl; never let it reach the compat arm.
        "bedrock" => None,
        #[cfg(feature = "vertex")]
        "vertex" => crate::providers::vertex::VertexAdapter::new(config)
            .ok()
            .map(|a| Arc::new(a) as Arc<dyn ProviderCatalog>),
        #[cfg(not(feature = "vertex"))]
        "vertex" => None,
        // Foundry lists the resource's deployments. `new` fails when the
        // provider table names no endpoint, which reports as "not supported"
        // rather than an error per fetch.
        #[cfg(feature = "foundry")]
        "foundry" => crate::providers::foundry::FoundryAdapter::new(config)
            .ok()
            .map(|a| Arc::new(a) as Arc<dyn ProviderCatalog>),
        #[cfg(not(feature = "foundry"))]
        "foundry" => None,
        // Catalog listing never dispatches via complete()/stream(), so the
        // per-tier lookup this adapter also carries is never consulted here
        // — a default is fine.
        "anthropic" => Some(Arc::new(crate::providers::anthropic::AnthropicAdapter::new(
            config,
            crate::config::schema::TierTimeoutsConfig::default(),
        ))),
        _ => Some(Arc::new(crate::providers::openai_compat::OpenAICompatAdapter::new(
            config,
            crate::config::schema::TierTimeoutsConfig::default(),
        ))),
    }
}

/// Fan out over every configured provider's catalog concurrently. Each
/// provider degrades independently: an unreachable catalog yields an `error`
/// entry for that provider without failing the whole response. Search-only
/// providers have no model catalog: they contribute no entry and no request.
pub async fn aggregate_catalogs(providers: &HashMap<String, ProviderConfig>) -> serde_json::Value {
    let mut names: Vec<&String> = providers
        .keys()
        .filter(|name| !crate::providers::search_registry::is_search_only(name))
        .collect();
    names.sort();

    let fetches = names.iter().map(|name| {
        let config = &providers[*name];
        let catalog = catalog_for(name, config);
        async move {
            let entry = match catalog {
                None => serde_json::json!({ "supported": false }),
                Some(cat) => match cat.list_models().await {
                    Ok(models) => {
                        // The compat adapter leaves `provider` empty (it serves
                        // many registry names); stamp the key we queried.
                        let stamped: Vec<serde_json::Value> = models
                            .into_iter()
                            .map(|mut m| {
                                if m.provider.is_empty() {
                                    m.provider = name.to_string();
                                }
                                serde_json::to_value(m).unwrap_or_default()
                            })
                            .collect();
                        serde_json::json!({ "supported": true, "models": stamped })
                    }
                    Err(e) => serde_json::json!({ "supported": true, "error": e.to_string() }),
                },
            };
            (name.to_string(), entry)
        }
    });

    let results = join_all(fetches).await;
    let mut map = serde_json::Map::new();
    for (name, entry) in results {
        map.insert(name, entry);
    }
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json, Router};

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    fn compat_config(base: &str) -> ProviderConfig {
        let mut c = ProviderConfig::default();
        c.api_base = Some(base.to_string());
        c.api_key = "k".into();
        c
    }

    #[tokio::test]
    async fn aggregates_with_per_provider_degradation() {
        let good = serve(Router::new().route(
            "/models",
            get(|| async { Json(serde_json::json!({"data": [{"id": "gpt-4o"}]})) }),
        ))
        .await;
        let bad = serve(Router::new().route(
            "/models",
            get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
        ))
        .await;

        let mut providers = HashMap::new();
        providers.insert("openai".to_string(), compat_config(&good));
        providers.insert("groq".to_string(), compat_config(&bad));
        providers.insert("azure".to_string(), ProviderConfig::default());

        let out = aggregate_catalogs(&providers).await;

        assert_eq!(out["azure"]["supported"], false);
        assert_eq!(out["openai"]["supported"], true);
        assert_eq!(out["openai"]["models"][0]["name"], "gpt-4o");
        // Empty provider stamped with the registry key that was queried.
        assert_eq!(out["openai"]["models"][0]["provider"], "openai");
        assert_eq!(out["groq"]["supported"], true);
        assert!(out["groq"]["error"].as_str().unwrap().contains("500"));
        assert!(out["groq"].get("models").is_none());
    }

    /// A search-only provider configured beside an OpenAI-compatible one:
    /// only the latter is aggregated, the search provider's base never sees a
    /// request, and the result carries no entry (so no error) for it.
    #[tokio::test]
    async fn search_only_providers_have_no_catalog_entry_and_no_request() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let good = serve(Router::new().route(
            "/models",
            get(|| async { Json(serde_json::json!({"data": [{"id": "gpt-4o"}]})) }),
        ))
        .await;
        // Every path on the search providers' base counts a hit and answers
        // 401, so a request reaching it shows up here and as an error entry.
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let search_base = serve(Router::new().fallback(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                (axum::http::StatusCode::UNAUTHORIZED, "no key")
            }
        }))
        .await;

        let mut providers = HashMap::new();
        providers.insert("openai".to_string(), compat_config(&good));
        for search in ["bing_grounding", "tavily"] {
            let mut c = ProviderConfig::default();
            c.api_base = Some(search_base.clone());
            providers.insert(search.to_string(), c);
        }

        let out = aggregate_catalogs(&providers).await;

        let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["openai"], "only the model provider is aggregated: {out}");
        assert_eq!(out["openai"]["models"][0]["name"], "gpt-4o");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "a catalog request reached a search-only provider");
    }

    #[test]
    fn search_only_names_never_reach_the_compat_arm() {
        assert!(catalog_for("bing_grounding", &ProviderConfig::default()).is_none());
        assert!(catalog_for("tavily", &ProviderConfig::default()).is_none());
        assert!(!crate::providers::search_registry::is_search_only("vertex"));
    }

    #[test]
    fn feature_gated_names_never_reach_the_compat_arm() {
        // bedrock always; vertex when compiled out — both must be explicit
        // None/Some(vertex), not an OpenAI-compat catalog (issue #24's rule).
        assert!(catalog_for("bedrock", &ProviderConfig::default()).is_none());
        #[cfg(not(feature = "vertex"))]
        assert!(catalog_for("vertex", &ProviderConfig::default()).is_none());
        // "foundry" resolves to None either way with an empty config —
        // compiled out by the explicit arm, compiled in because construction
        // needs an endpoint. What must never happen is a compat catalog.
        assert!(catalog_for("foundry", &ProviderConfig::default()).is_none());
    }
}
