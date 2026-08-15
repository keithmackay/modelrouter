//! Catalog aggregation across configured providers (issue #34).
//!
//! `catalog_for` mirrors `registry.rs`'s adapter dispatch — including its
//! hard-won rule from #24: a feature-gated provider name must NEVER fall
//! through to the OpenAI-compat arm. Providers without a catalog surface
//! return `None`, which the aggregate reports as "not supported" rather than
//! an empty model list.

use crate::config::schema::ProviderConfig;
use crate::providers::catalog::ProviderCatalog;
use futures::future::join_all;
use std::collections::HashMap;
use std::sync::Arc;

/// Catalog capability for one configured provider, or `None` when the
/// provider has no catalog surface in this binary.
pub fn catalog_for(provider_name: &str, config: &ProviderConfig) -> Option<Arc<dyn ProviderCatalog>> {
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
        "anthropic" => Some(Arc::new(crate::providers::anthropic::AnthropicAdapter::new(config))),
        _ => Some(Arc::new(crate::providers::openai_compat::OpenAICompatAdapter::new(config))),
    }
}

/// Fan out over every configured provider's catalog concurrently. Each
/// provider degrades independently: an unreachable catalog yields an `error`
/// entry for that provider without failing the whole response.
pub async fn aggregate_catalogs(providers: &HashMap<String, ProviderConfig>) -> serde_json::Value {
    let mut names: Vec<&String> = providers.keys().collect();
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

    #[test]
    fn feature_gated_names_never_reach_the_compat_arm() {
        // bedrock always; vertex when compiled out — both must be explicit
        // None/Some(vertex), not an OpenAI-compat catalog (issue #24's rule).
        assert!(catalog_for("bedrock", &ProviderConfig::default()).is_none());
        #[cfg(not(feature = "vertex"))]
        assert!(catalog_for("vertex", &ProviderConfig::default()).is_none());
    }
}
