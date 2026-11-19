//! The streaming transcription adapter, built lazily from `[transcription]`
//! and the provider table the way [`crate::providers::decision_registry`]
//! builds decision adapters. One provider serves the route (a session is a
//! long-lived socket to one backend); which providers CAN is the match arm
//! below, so a new backend (a speech service, a self-hosted model) is one arm
//! and one adapter, never a change to the route.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tokio::sync::Semaphore;

use crate::config::schema::{ProviderConfig, TranscriptionConfig};
use crate::providers::transcription::StreamingTranscriptionAdapter;

/// Why a session cannot be served, before any provider socket opens. The
/// route turns each into the `error` message the caller sees.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptionUnavailable {
    #[error("streaming transcription is not configured: set [transcription] provider to the provider that serves it")]
    NotConfigured,
    #[error("provider \"{provider}\" is named by [transcription] but [providers.{provider}] is missing")]
    ProviderMissing { provider: String },
    #[error("provider \"{provider}\" does not serve streaming transcription")]
    Unsupported { provider: String },
    #[error("provider \"{provider}\" could not build its transcription adapter: {source}")]
    Build { provider: String, #[source] source: anyhow::Error },
    #[error("this replica already serves its {max_sessions} concurrent transcription sessions ([transcription] max_sessions)")]
    Busy { max_sessions: usize },
}

/// A build outcome worth remembering: the registry reports the same failure
/// on every later session instead of rebuilding (and re-logging) it.
#[derive(Debug, Clone)]
enum RememberedFailure {
    ProviderMissing { provider: String },
    Unsupported { provider: String },
    Build { provider: String, message: String },
}

impl RememberedFailure {
    fn remember(err: TranscriptionUnavailable) -> Self {
        match err {
            TranscriptionUnavailable::ProviderMissing { provider } => Self::ProviderMissing { provider },
            TranscriptionUnavailable::Unsupported { provider } => Self::Unsupported { provider },
            TranscriptionUnavailable::Build { provider, source } => Self::Build { provider, message: format!("{source:#}") },
            // Neither comes out of a build; keep the message if one ever does.
            other @ (TranscriptionUnavailable::NotConfigured | TranscriptionUnavailable::Busy { .. }) => {
                Self::Build { provider: String::new(), message: other.to_string() }
            }
        }
    }

    fn recall(&self) -> TranscriptionUnavailable {
        match self {
            Self::ProviderMissing { provider } => TranscriptionUnavailable::ProviderMissing { provider: provider.clone() },
            Self::Unsupported { provider } => TranscriptionUnavailable::Unsupported { provider: provider.clone() },
            Self::Build { provider, message } => TranscriptionUnavailable::Build {
                provider: provider.clone(),
                source: anyhow::anyhow!("{message}"),
            },
        }
    }
}

pub struct TranscriptionRegistry {
    config: TranscriptionConfig,
    providers: HashMap<String, ProviderConfig>,
    adapter: OnceLock<Result<Arc<dyn StreamingTranscriptionAdapter>, RememberedFailure>>,
    /// Session admission per replica (`max_sessions`); `None` = unlimited.
    sessions: Option<Arc<Semaphore>>,
}

impl TranscriptionRegistry {
    pub fn new(config: TranscriptionConfig, providers: HashMap<String, ProviderConfig>) -> Self {
        let sessions = (config.max_sessions > 0).then(|| Arc::new(Semaphore::new(config.max_sessions)));
        Self { config, providers, adapter: OnceLock::new(), sessions }
    }

    /// Pin an adapter under the configured provider name (tests pin a
    /// fake-upstream adapter here; production adapters are built in
    /// [`Self::adapter`]). The registry's `[transcription] provider` must name
    /// `provider`, which keeps the test wiring honest about the config path.
    pub fn with_adapter<A: StreamingTranscriptionAdapter + 'static>(self, provider: &str, adapter: A) -> Self {
        assert_eq!(
            self.config.provider_name(),
            Some(provider),
            "the pinned adapter's provider must be the configured [transcription] provider"
        );
        let _ = self.adapter.set(Ok(Arc::new(adapter)));
        self
    }

    pub fn config(&self) -> &TranscriptionConfig {
        &self.config
    }

    /// The configured provider's name, or `NotConfigured`.
    pub fn provider_name(&self) -> Result<&str, TranscriptionUnavailable> {
        self.config.provider_name().ok_or(TranscriptionUnavailable::NotConfigured)
    }

    /// The adapter, built on first use. A build failure is remembered and
    /// reported the same way on every later session, so a misconfiguration
    /// is one log line per process rather than one per attempt.
    pub fn adapter(&self) -> Result<Arc<dyn StreamingTranscriptionAdapter>, TranscriptionUnavailable> {
        let provider = self.provider_name()?.to_string();
        let built = self.adapter.get_or_init(|| self.build(&provider).map_err(RememberedFailure::remember));
        match built {
            Ok(adapter) => Ok(adapter.clone()),
            Err(failure) => Err(failure.recall()),
        }
    }

    fn build(&self, provider: &str) -> Result<Arc<dyn StreamingTranscriptionAdapter>, TranscriptionUnavailable> {
        let config = self
            .providers
            .get(provider)
            .ok_or_else(|| TranscriptionUnavailable::ProviderMissing { provider: provider.to_string() })?;
        match provider {
            #[cfg(feature = "foundry")]
            "foundry" => crate::providers::foundry::transcribe::FoundryTranscriptionAdapter::new(config, &self.config)
                .map(|a| Arc::new(a) as Arc<dyn StreamingTranscriptionAdapter>)
                .map_err(|source| TranscriptionUnavailable::Build { provider: provider.to_string(), source }),
            _ => Err(TranscriptionUnavailable::Unsupported { provider: provider.to_string() }),
        }
    }

    /// Admit a session under `max_sessions`; `Ok(None)` when unlimited.
    pub fn admit(&self) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, TranscriptionUnavailable> {
        let Some(sessions) = &self.sessions else { return Ok(None) };
        sessions.clone().try_acquire_owned().map(Some).map_err(|_| TranscriptionUnavailable::Busy {
            max_sessions: self.config.max_sessions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(provider: Option<&str>) -> TranscriptionConfig {
        TranscriptionConfig { provider: provider.map(str::to_string), ..Default::default() }
    }

    #[test]
    fn unconfigured_is_not_configured() {
        let registry = TranscriptionRegistry::new(config(None), HashMap::new());
        assert!(matches!(registry.adapter().err().expect("an error"), TranscriptionUnavailable::NotConfigured));
        // Blank counts as unset.
        let registry = TranscriptionRegistry::new(config(Some("  ")), HashMap::new());
        assert!(matches!(registry.adapter().err().expect("an error"), TranscriptionUnavailable::NotConfigured));
    }

    #[test]
    fn a_provider_without_a_table_entry_is_named() {
        let registry = TranscriptionRegistry::new(config(Some("foundry")), HashMap::new());
        let err = registry.adapter().err().expect("an error");
        assert!(matches!(&err, TranscriptionUnavailable::ProviderMissing { provider } if provider == "foundry"), "{err}");
    }

    #[test]
    fn a_provider_that_cannot_stream_is_unsupported() {
        let mut providers = HashMap::new();
        providers.insert("openai".to_string(), ProviderConfig::default());
        let registry = TranscriptionRegistry::new(config(Some("openai")), providers);
        let err = registry.adapter().err().expect("an error");
        assert!(matches!(&err, TranscriptionUnavailable::Unsupported { provider } if provider == "openai"), "{err}");
    }

    #[cfg(feature = "foundry")]
    #[test]
    fn a_build_failure_is_reported_once_and_remembered() {
        // foundry with no endpoint cannot build; the second call must not
        // rebuild, so the same message comes back.
        let mut providers = HashMap::new();
        providers.insert("foundry".to_string(), ProviderConfig::default());
        let registry = TranscriptionRegistry::new(config(Some("foundry")), providers);
        let first = registry.adapter().err().expect("an error").to_string();
        let second = registry.adapter().err().expect("an error").to_string();
        assert!(first.contains("foundry_endpoint"), "{first}");
        assert_eq!(first, second);
    }

    #[test]
    fn admission_is_bounded_by_max_sessions() {
        let mut cfg = config(Some("foundry"));
        cfg.max_sessions = 1;
        let registry = TranscriptionRegistry::new(cfg, HashMap::new());
        let permit = registry.admit().unwrap();
        assert!(permit.is_some());
        let err = registry.admit().unwrap_err();
        assert!(matches!(err, TranscriptionUnavailable::Busy { max_sessions: 1 }), "{err}");
        drop(permit);
        assert!(registry.admit().unwrap().is_some());
        // 0 = unlimited: no permit, never busy.
        let registry = TranscriptionRegistry::new(config(Some("foundry")), HashMap::new());
        assert!(registry.admit().unwrap().is_none());
    }
}
