pub mod adapter;
pub mod catalog;
pub mod catalog_registry;
pub mod anthropic;
#[cfg(feature = "bedrock")]
pub mod bedrock;
pub mod azure_openai;
pub mod embed_registry;
pub mod embedding;
pub mod openai_compat;
pub mod openai_embed;
pub mod openai_images;
pub mod registry;
pub mod search;
pub mod search_registry;
pub mod tavily;
#[cfg(feature = "vertex")]
pub mod vertex;

/// Provider names whose adapters are compiled behind a cargo feature, paired
/// with whether that feature is present in this binary. A config that names
/// one of these while the feature is absent must be a startup error: the
/// request-path registries would otherwise fall through to the OpenAI-compat
/// adapter and silently send every call — prompts included — to a provider
/// the operator never configured (issue #24).
const FEATURE_GATED_PROVIDERS: &[(&str, bool)] = &[
    ("vertex", cfg!(feature = "vertex")),
    ("bedrock", cfg!(feature = "bedrock")),
];

/// Refuse to start when the config names a provider whose adapter is not in
/// this binary. Called from `serve` before any registry is built, so the
/// failure happens once, loudly, at boot — not per request.
pub fn validate_provider_features(
    configs: &std::collections::HashMap<String, crate::config::schema::ProviderConfig>,
) -> anyhow::Result<()> {
    let missing: Vec<&str> = FEATURE_GATED_PROVIDERS
        .iter()
        .filter(|(name, compiled)| !compiled && configs.contains_key(*name))
        .map(|(name, _)| *name)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let features = missing.join(",");
    anyhow::bail!(
        "config declares provider(s) {} but this binary was built without the matching cargo feature(s) — \
         rebuild with `cargo build --release --features {}`. Refusing to start: without the feature these \
         providers would silently fall back to the OpenAI-compat adapter and route requests to the wrong service.",
        missing
            .iter()
            .map(|m| format!("\"{m}\""))
            .collect::<Vec<_>>()
            .join(", "),
        features,
    )
}

#[cfg(test)]
mod feature_gate_tests {
    use super::validate_provider_features;
    use crate::config::schema::ProviderConfig;
    use std::collections::HashMap;

    fn configs(names: &[&str]) -> HashMap<String, ProviderConfig> {
        names
            .iter()
            .map(|n| (n.to_string(), ProviderConfig::default()))
            .collect()
    }

    #[test]
    fn ungated_providers_always_pass() {
        assert!(validate_provider_features(&configs(&["openai", "anthropic", "azure"])).is_ok());
    }

    #[cfg(feature = "vertex")]
    #[test]
    fn vertex_passes_when_compiled_in() {
        assert!(validate_provider_features(&configs(&["vertex"])).is_ok());
    }

    #[cfg(not(feature = "vertex"))]
    #[test]
    fn vertex_fails_when_compiled_out() {
        let err = validate_provider_features(&configs(&["vertex"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("\"vertex\""), "{msg}");
        assert!(msg.contains("--features vertex"), "{msg}");
    }

    #[cfg(not(feature = "bedrock"))]
    #[test]
    fn bedrock_fails_when_compiled_out() {
        let err = validate_provider_features(&configs(&["bedrock"])).unwrap_err();
        assert!(err.to_string().contains("\"bedrock\""), "{err}");
    }

    #[cfg(all(not(feature = "vertex"), not(feature = "bedrock")))]
    #[test]
    fn all_missing_features_are_listed_together() {
        let err = validate_provider_features(&configs(&["vertex", "bedrock", "openai"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("\"vertex\"") && msg.contains("\"bedrock\""), "{msg}");
    }
}
