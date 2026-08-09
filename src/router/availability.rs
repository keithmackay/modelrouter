//! Operator-controlled availability of models and providers (issue #5).
//!
//! # Relationship to the circuit breaker
//!
//! These are two deliberately different mechanisms and they do not share state:
//!
//! | | Circuit breaker ([`crate::router::circuit_breaker`]) | Operator disable (here) |
//! |---|---|---|
//! | Trigger | observed provider failures | an explicit admin action |
//! | Scope | provider | provider *or* a single model |
//! | Lifetime | auto-recovers after a cooldown | **sticky** until explicitly re-enabled |
//! | Storage | in-memory, per process | persisted, survives restart |
//! | Direct request | falls through the fallback chain, then 502 | 403 naming the reason |
//!
//! A disable is checked *before* the breaker and before any provider dispatch, so a
//! disabled entity is never called and therefore can neither trip nor reset a breaker.
//! A breaker trip is a transient "this is unhealthy right now" signal; a disable is a
//! durable "an operator has taken this out of rotation" decision, which is why it is
//! never cleared by a successful call or by time.
//!
//! Interaction with routing:
//! - **Direct request** for a disabled model/provider → 403 naming the reason, so an
//!   operator's cost or security decision is never silently rerouted or reported as a
//!   provider fault.
//! - **Fallback chains** skip disabled candidates and continue down the chain.
//! - **Load-balancer pools** skip disabled entries when selecting.
//! - **Aliases** resolving onto a disabled target produce the same 403.
//! - **`/v1/models`** omits disabled models and disabled providers.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Who disabled something, when, and why.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct DisableInfo {
    pub reason: Option<String>,
    pub by: Option<String>,
    pub at: Option<String>,
}

/// A disabled model or provider that a request tried to use.
#[derive(Clone, Debug, PartialEq)]
pub struct Unavailable {
    /// "model" or "provider".
    pub scope: &'static str,
    /// `provider/model` for a model, the provider name for a provider.
    pub target: String,
    pub info: DisableInfo,
}

impl Unavailable {
    /// Operator-facing message returned to the caller. Always names the reason.
    pub fn message(&self) -> String {
        let reason = self
            .info
            .reason
            .as_deref()
            .filter(|r| !r.trim().is_empty())
            .unwrap_or("no reason recorded");
        let mut msg = format!(
            "{} '{}' has been disabled by an administrator: {}",
            self.scope, self.target, reason
        );
        if let Some(by) = self.info.by.as_deref().filter(|b| !b.is_empty()) {
            msg.push_str(&format!(" (disabled by {by}"));
            if let Some(at) = self.info.at.as_deref().filter(|a| !a.is_empty()) {
                msg.push_str(&format!(" at {at}"));
            }
            msg.push(')');
        }
        msg
    }
}

/// Snapshot of everything an operator has taken out of rotation.
#[derive(Clone, Debug, Default)]
pub struct AvailabilityMap {
    /// Keyed by `provider/model`.
    models: HashMap<String, DisableInfo>,
    /// Keyed by provider name.
    providers: HashMap<String, DisableInfo>,
}

impl AvailabilityMap {
    pub fn new(
        models: HashMap<String, DisableInfo>,
        providers: HashMap<String, DisableInfo>,
    ) -> Self {
        Self { models, providers }
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty() && self.providers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.models.len() + self.providers.len()
    }

    pub fn disabled_models(&self) -> &HashMap<String, DisableInfo> {
        &self.models
    }

    pub fn disabled_providers(&self) -> &HashMap<String, DisableInfo> {
        &self.providers
    }

    /// A disabled provider disables every model under it, so the provider is checked first.
    pub fn check(&self, provider: &str, model: &str) -> Result<(), Unavailable> {
        if let Some(info) = self.providers.get(provider) {
            return Err(Unavailable {
                scope: "provider",
                target: provider.to_string(),
                info: info.clone(),
            });
        }
        let key = format!("{provider}/{model}");
        if let Some(info) = self.models.get(&key) {
            return Err(Unavailable {
                scope: "model",
                target: key,
                info: info.clone(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(reason: &str) -> DisableInfo {
        DisableInfo {
            reason: Some(reason.to_string()),
            by: Some("ops".to_string()),
            at: Some("2026-08-08T00:00:00Z".to_string()),
        }
    }

    #[test]
    fn empty_map_allows_everything() {
        let m = AvailabilityMap::default();
        assert!(m.is_empty());
        assert!(m.check("openai", "gpt-5").is_ok());
    }

    #[test]
    fn disabled_model_is_reported_with_scope_and_reason() {
        let m = AvailabilityMap::new(
            HashMap::from([("openai/gpt-5".to_string(), info("cost spike"))]),
            HashMap::new(),
        );
        let err = m.check("openai", "gpt-5").unwrap_err();
        assert_eq!(err.scope, "model");
        assert_eq!(err.target, "openai/gpt-5");
        assert!(err.message().contains("cost spike"));
        assert!(err.message().contains("ops"));
        // A different model on the same provider is unaffected.
        assert!(m.check("openai", "gpt-5-mini").is_ok());
    }

    #[test]
    fn disabled_provider_disables_all_its_models() {
        let m = AvailabilityMap::new(
            HashMap::new(),
            HashMap::from([("anthropic".to_string(), info("vendor incident"))]),
        );
        let err = m.check("anthropic", "claude-opus-4-6").unwrap_err();
        assert_eq!(err.scope, "provider");
        assert_eq!(err.target, "anthropic");
        assert!(err.message().contains("vendor incident"));
        assert!(m.check("openai", "gpt-5").is_ok());
    }

    #[test]
    fn provider_disable_takes_precedence_over_model_disable() {
        let m = AvailabilityMap::new(
            HashMap::from([("openai/gpt-5".to_string(), info("model reason"))]),
            HashMap::from([("openai".to_string(), info("provider reason"))]),
        );
        assert_eq!(m.check("openai", "gpt-5").unwrap_err().scope, "provider");
    }

    #[test]
    fn message_is_clear_when_no_reason_was_recorded() {
        let m = AvailabilityMap::new(
            HashMap::from([("openai/gpt-5".to_string(), DisableInfo::default())]),
            HashMap::new(),
        );
        let msg = m.check("openai", "gpt-5").unwrap_err().message();
        assert!(msg.contains("no reason recorded"), "{msg}");
        assert!(!msg.contains("disabled by )"), "{msg}");
    }
}
