//! Per-model sampling-parameter capabilities.
//!
//! Providers do not accept a uniform parameter set across their own model
//! families, and they change that set between versions. Anthropic's Claude 5
//! family rejects `temperature` outright — Vertex answers a request carrying
//! one with:
//!
//! ```text
//! 400 Bad Request
//! {"type":"error","error":{"type":"invalid_request_error",
//!  "message":"`temperature` is deprecated for this model."}}
//! ```
//!
//! while `claude-haiku-4-5`, behind the same provider, still honours it. A
//! caller addressing a routing alias (`deep`, `balanced`) cannot know which of
//! those it will reach — the alias exists precisely so it doesn't have to. The
//! router resolves the alias, so the router is the only component positioned to
//! know whether the resolved model accepts the parameter, and it strips the
//! ones the model would reject.
//!
//! Defaults below are empirically probed, not inferred from version numbers.
//! Operators override them from config without waiting on a release:
//!
//! ```toml
//! [[model_capabilities]]
//! model = "claude-opus-6"
//! supports_temperature = false
//! ```

use crate::config::schema::ModelCapabilityEntry;

/// Models known to reject `temperature`. Probed against Vertex on 2026-09-01;
/// every entry here returned `invalid_request_error` for `temperature: 0.3`
/// and succeeded with the parameter absent.
///
/// Keys are normalized model names (provider prefix stripped, lowercased) —
/// see [`normalize_model_key`].
const TEMPERATURE_UNSUPPORTED: &[&str] = &[
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-fable-5-1",
];

/// Reduce a routed model name to the key used for capability lookup.
///
/// Strips a leading provider segment (`anthropic/claude-opus-5` →
/// `claude-opus-5`) and lowercases, matching how
/// [`crate::router::cost::CostCalculator`] keys its pricing table so operators
/// write the same model string in both config blocks.
fn normalize_model_key(model: &str) -> String {
    let key = match model.find('/') {
        Some(pos) => &model[pos + 1..],
        None => model,
    };
    key.to_lowercase()
}

/// Drop a Vertex-style `@YYYYMMDD` version suffix, if present.
///
/// Claude-on-Vertex is addressed both ways in practice — the alias table on
/// this deployment holds a bare `claude-fable-5-1` while the config example
/// pins `claude-opus-4-5@20251101` — and a parameter a model family rejects is
/// rejected by every snapshot of it. Matching only the exact string would let a
/// pinned deployment sail past the table it is listed in.
fn strip_version(key: &str) -> &str {
    match key.find('@') {
        Some(pos) => &key[..pos],
        None => key,
    }
}

/// Whether `model` accepts a `temperature` sampling parameter.
///
/// A config entry for the model wins over the built-in table, so an operator
/// can both *add* a model the build doesn't know about and *retract* a built-in
/// entry once a provider restores support. Unknown models are assumed to
/// support it: the router must not silently drop parameters it has no evidence
/// are unwelcome.
///
/// Lookup tries the fully qualified name before the version-stripped one, so a
/// version-pinned entry overrides a family-wide one rather than the reverse.
pub fn supports_temperature(model: &str, overrides: &[ModelCapabilityEntry]) -> bool {
    let key = normalize_model_key(model);
    let base = strip_version(&key);

    let mut candidates = vec![key.as_str()];
    if base != key {
        candidates.push(base);
    }

    for candidate in &candidates {
        for entry in overrides {
            if normalize_model_key(&entry.model) == *candidate {
                if let Some(supported) = entry.supports_temperature {
                    return supported;
                }
            }
        }
    }

    !candidates
        .iter()
        .any(|c| TEMPERATURE_UNSUPPORTED.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(model: &str, supports_temperature: Option<bool>) -> ModelCapabilityEntry {
        ModelCapabilityEntry { model: model.to_string(), supports_temperature }
    }

    #[test]
    fn unknown_models_are_assumed_to_support_temperature() {
        assert!(supports_temperature("some-future-model", &[]));
        assert!(supports_temperature("gpt-4o", &[]));
    }

    #[test]
    fn built_in_table_covers_the_claude_5_family() {
        assert!(!supports_temperature("claude-opus-5", &[]));
        assert!(!supports_temperature("claude-sonnet-5", &[]));
        assert!(!supports_temperature("claude-fable-5-1", &[]));
    }

    /// The 4.5 model sits behind the same provider as the 5 family and still
    /// honours `temperature` — the reason this is a per-model table and not a
    /// per-provider switch.
    #[test]
    fn same_provider_older_model_keeps_temperature() {
        assert!(supports_temperature("claude-haiku-4-5", &[]));
        assert!(supports_temperature("anthropic/claude-haiku-4-5", &[]));
    }

    #[test]
    fn provider_prefix_and_case_do_not_affect_lookup() {
        assert!(!supports_temperature("anthropic/claude-opus-5", &[]));
        assert!(!supports_temperature("Anthropic/Claude-Opus-5", &[]));
        assert!(!supports_temperature("CLAUDE-OPUS-5", &[]));
    }

    #[test]
    fn config_adds_a_model_the_build_does_not_know() {
        let overrides = vec![entry("claude-opus-6", Some(false))];
        assert!(!supports_temperature("claude-opus-6", &overrides));
        assert!(!supports_temperature("anthropic/claude-opus-6", &overrides));
    }

    /// The override must be able to run the other way too, so a provider
    /// restoring support doesn't require a new build to exploit.
    #[test]
    fn config_retracts_a_built_in_entry() {
        let overrides = vec![entry("claude-opus-5", Some(true))];
        assert!(supports_temperature("claude-opus-5", &overrides));
    }

    #[test]
    fn entry_without_an_opinion_falls_through_to_the_built_in_table() {
        let overrides = vec![entry("claude-opus-5", None)];
        assert!(!supports_temperature("claude-opus-5", &overrides));
    }

    /// Vertex pins Claude snapshots as `<model>@<date>`. The parameter a family
    /// rejects is rejected by every snapshot of it, so the version-pinned form
    /// must resolve to the same answer as the bare one.
    #[test]
    fn a_version_pinned_model_matches_its_family_entry() {
        assert!(!supports_temperature("claude-opus-5@20260101", &[]));
        assert!(!supports_temperature("anthropic/claude-fable-5-1@20260214", &[]));
        assert!(supports_temperature("anthropic/claude-haiku-4-5@20251001", &[]));

        let overrides = vec![entry("claude-opus-6", Some(false))];
        assert!(!supports_temperature("anthropic/claude-opus-6@20260601", &overrides));
    }

    /// A pinned entry is more specific than a family-wide one and wins, so an
    /// operator can carve out the single snapshot that behaves differently.
    #[test]
    fn a_version_pinned_entry_overrides_the_family_wide_one() {
        let overrides = vec![
            entry("claude-opus-6", Some(false)),
            entry("claude-opus-6@20260601", Some(true)),
        ];
        assert!(supports_temperature("claude-opus-6@20260601", &overrides));
        assert!(!supports_temperature("claude-opus-6@20260101", &overrides));
        assert!(!supports_temperature("claude-opus-6", &overrides));
    }

    #[test]
    fn entries_for_other_models_do_not_leak() {
        let overrides = vec![entry("claude-haiku-4-5", Some(false))];
        assert!(!supports_temperature("claude-haiku-4-5", &overrides));
        assert!(supports_temperature("gpt-4o", &overrides));
    }
}
