pub mod adapter;
pub mod catalog;
pub mod catalog_registry;
pub mod anthropic;
/// Microsoft Entra ID token source, shared by every Azure provider that
/// authenticates without a key. Compiled in when any of them is.
#[cfg(any(feature = "bing-grounding", feature = "foundry"))]
pub mod azure_entra;
#[cfg(feature = "bedrock")]
pub mod bedrock;
#[cfg(feature = "bing-grounding")]
pub mod bing_grounding;
pub mod azure_openai;
pub mod embed_registry;
pub mod embedding;
#[cfg(feature = "foundry")]
pub mod foundry;
pub mod openai_compat;
pub mod openai_embed;
pub mod openai_images;
pub mod registry;
pub mod search;
pub mod search_registry;
pub mod tavily;
#[cfg(feature = "vertex")]
pub mod vertex;

/// Provider names whose adapters are compiled behind a cargo feature, as
/// `(provider name in config, cargo feature name, present in this binary)`.
/// The provider and feature names are carried separately because they do not
/// always coincide — `[providers.bing_grounding]` is built by
/// `--features bing-grounding` — and a fix-hint naming a feature that does not
/// exist is worse than no hint.
///
/// A config that names one of these while the feature is absent must be a
/// startup error: the request-path registries would otherwise fall through to
/// the OpenAI-compat adapter and silently send every call — prompts included —
/// to a provider the operator never configured (issue #24).
const FEATURE_GATED_PROVIDERS: &[(&str, &str, bool)] = &[
    ("vertex", "vertex", cfg!(feature = "vertex")),
    ("bedrock", "bedrock", cfg!(feature = "bedrock")),
    (
        "bing_grounding",
        "bing-grounding",
        cfg!(feature = "bing-grounding"),
    ),
    ("foundry", "foundry", cfg!(feature = "foundry")),
];

/// Refuse to start when the config names a provider whose adapter is not in
/// this binary. Called from `serve` before any registry is built, so the
/// failure happens once, loudly, at boot — not per request.
pub fn validate_provider_features(
    configs: &std::collections::HashMap<String, crate::config::schema::ProviderConfig>,
) -> anyhow::Result<()> {
    let missing: Vec<(&str, &str)> = FEATURE_GATED_PROVIDERS
        .iter()
        .filter(|(name, _, compiled)| !compiled && configs.contains_key(*name))
        .map(|(name, feature, _)| (*name, *feature))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let features = missing
        .iter()
        .map(|(_, feature)| *feature)
        .collect::<Vec<_>>()
        .join(",");
    anyhow::bail!(
        "config declares provider(s) {} but this binary was built without the matching cargo feature(s) — \
         rebuild with `cargo build --release --features {}`. Refusing to start: without the feature these \
         providers would silently fall back to the OpenAI-compat adapter and route requests to the wrong service.",
        missing
            .iter()
            .map(|(name, _)| format!("\"{name}\""))
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

    #[cfg(feature = "bing-grounding")]
    #[test]
    fn bing_grounding_passes_when_compiled_in() {
        assert!(validate_provider_features(&configs(&["bing_grounding"])).is_ok());
    }

    /// The provider is `bing_grounding`, the cargo feature is `bing-grounding`.
    /// The fix-hint must print the FEATURE name, or the operator pastes a
    /// command that fails with "none of the selected packages contains these
    /// features".
    #[cfg(not(feature = "bing-grounding"))]
    #[test]
    fn bing_grounding_hint_names_the_hyphenated_feature() {
        let err = validate_provider_features(&configs(&["bing_grounding"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("\"bing_grounding\""), "{msg}");
        assert!(msg.contains("--features bing-grounding"), "{msg}");
    }

    #[cfg(feature = "foundry")]
    #[test]
    fn foundry_passes_when_compiled_in() {
        assert!(validate_provider_features(&configs(&["foundry"])).is_ok());
    }

    #[cfg(not(feature = "foundry"))]
    #[test]
    fn foundry_fails_when_compiled_out() {
        let err = validate_provider_features(&configs(&["foundry"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("\"foundry\""), "{msg}");
        assert!(msg.contains("--features foundry"), "{msg}");
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
