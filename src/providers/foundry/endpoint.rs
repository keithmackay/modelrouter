//! Endpoint, surface, api-version and Entra-scope resolution for the
//! `foundry` provider. Shared by the chat adapter, the embedding adapter and
//! the catalog so all three agree on where they are pointing and what audience
//! they are authenticating to.
//!
//! ## Two surfaces, one provider
//!
//! An Azure AI Foundry (AI Services) resource serves deployed models on two
//! documented data planes, and they differ in path, api-version rules and
//! nothing else that matters here:
//!
//! | Surface | Base | api-version |
//! |---|---|---|
//! | OpenAI-compatible (`OpenAIV1`) | `{endpoint}/openai/v1` | optional, defaults to `v1` |
//! | Azure AI Model Inference (`ModelInference`) | `{endpoint}/models` | REQUIRED |
//!
//! Sources: `specification/ai/data-plane/OpenAI.v1/azure-v1-v1-generated.yaml`
//! (`servers: {endpoint}/openai/v1`, `api-version` optional with default `v1`)
//! and `specification/ai/data-plane/ModelInference/openapi/2024-05-01-preview/openapi.yaml`
//! (`servers: https://{resource}.services.ai.azure.com/models`, `api-version`
//! required) in Azure/azure-rest-api-specs.
//!
//! The Model Inference document now carries a deprecation notice — *"the Azure
//! AI Model Inference API is deprecated and will be retired in the future.
//! Migrate to the OpenAI API"* — so `OpenAIV1` is what a bare host resolves to.
//! An operator who needs the older surface says so by pasting the endpoint the
//! portal shows for it, which already ends in `/models`. The surface is the
//! path you configured; there is no separate switch to keep in sync with it.
//!
//! ## Scope per endpoint
//!
//! Both OpenAPI documents declare the OAuth2 scope
//! `https://cognitiveservices.azure.com/.default` for the resource-level
//! surfaces. A Foundry PROJECT endpoint (`/api/projects/<project>`) instead
//! takes `https://ai.azure.com/.default`, which is what the Agents/Responses
//! samples and `bing_grounding` use.
//!
//! Microsoft's current keyless-auth how-to also shows `https://ai.azure.com/.default`
//! for resource endpoints, i.e. the published spec and the published prose do
//! not agree. Rather than guess for every deployment, the derived scope is
//! overridable with `entra_scope` under `[providers.foundry]`, and the resolved
//! value is logged at construction and named in the 401 hint.

use crate::config::schema::ProviderConfig;
use crate::providers::azure_entra::{COGNITIVE_SERVICES_SCOPE, FOUNDRY_PROJECT_SCOPE};

/// Path segment of the OpenAI-compatible surface.
const OPENAI_V1_PATH: &str = "/openai/v1";

/// Path segment of the Azure AI Model Inference surface.
const MODEL_INFERENCE_PATH: &str = "/models";

/// Marks a Foundry project endpoint, which changes the Entra audience.
const PROJECTS_SEGMENT: &str = "/api/projects/";

/// api-version sent on the Model Inference surface, where the parameter is
/// required. `2025-04-01` is the value used throughout the current REST
/// reference samples; the OpenAPI document published in azure-rest-api-specs
/// is still stamped `2024-05-01-preview`. Preview versions get retired, so the
/// GA-shaped one is the default and `api_version` in config overrides it.
pub const DEFAULT_MODEL_INFERENCE_API_VERSION: &str = "2025-04-01";

/// Which data plane this provider is pointed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoundrySurface {
    /// `{endpoint}/openai/v1` — OpenAI-compatible chat/embeddings/models.
    OpenAIV1,
    /// `{endpoint}/models` — Azure AI Model Inference (deprecated upstream).
    ModelInference,
}

impl FoundrySurface {
    pub fn label(&self) -> &'static str {
        match self {
            Self::OpenAIV1 => "openai_v1",
            Self::ModelInference => "model_inference",
        }
    }
}

/// A resolved Foundry endpoint: base URL, surface, api-version and audience.
#[derive(Debug, Clone)]
pub struct FoundryEndpoint {
    /// Base URL including the surface path, no trailing slash.
    base: String,
    surface: FoundrySurface,
    api_version: Option<String>,
    scope: String,
}

impl FoundryEndpoint {
    /// Resolve from `[providers.foundry]`.
    ///
    /// `foundry_endpoint` is preferred, `api_base` accepted so the provider
    /// table reads like every other one. `project`, when set, appends the
    /// project segment — the same project/region split idiom the Vertex block
    /// uses, with the project naming a Foundry project instead of a GCP one.
    pub fn from_config(config: &ProviderConfig) -> anyhow::Result<Self> {
        let raw = config
            .foundry_endpoint
            .clone()
            .or_else(|| config.api_base.clone())
            .map(|e| e.trim().trim_end_matches('/').to_string())
            .filter(|e| !e.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "foundry needs `foundry_endpoint` (or `api_base`) under [providers.foundry] — \
                     the Azure AI Foundry endpoint from the portal, e.g. \
                     https://<resource>.services.ai.azure.com (the router appends /openai/v1), or \
                     the model-inference endpoint https://<resource>.services.ai.azure.com/models"
                )
            })?;

        let with_project = match config.project.as_deref().map(str::trim) {
            Some(project) if !project.is_empty() && !raw.contains(PROJECTS_SEGMENT) => {
                format!("{raw}{PROJECTS_SEGMENT}{project}")
            }
            _ => raw,
        };

        let (base, surface) = Self::resolve_surface(&with_project);

        let api_version = config
            .api_version
            .clone()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .or(match surface {
                // Required by the spec: never omitted, only overridden.
                FoundrySurface::ModelInference => {
                    Some(DEFAULT_MODEL_INFERENCE_API_VERSION.to_string())
                }
                // Optional, defaults to `v1` server-side. Sending nothing is
                // what keeps this surface version-less, matching bing_grounding.
                FoundrySurface::OpenAIV1 => None,
            });

        let scope = config
            .entra_scope
            .clone()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| Self::default_scope(&base).to_string());

        Ok(Self {
            base,
            surface,
            api_version,
            scope,
        })
    }

    /// Split a configured endpoint into (base including surface path, surface).
    ///
    /// A path the operator already wrote is honoured; a bare host gets the
    /// OpenAI-compatible surface appended, because the Model Inference API is
    /// documented as deprecated in favour of it.
    pub fn resolve_surface(endpoint: &str) -> (String, FoundrySurface) {
        let trimmed = endpoint.trim_end_matches('/');
        if trimmed.contains(OPENAI_V1_PATH) {
            (trimmed.to_string(), FoundrySurface::OpenAIV1)
        } else if trimmed.ends_with(MODEL_INFERENCE_PATH) {
            (trimmed.to_string(), FoundrySurface::ModelInference)
        } else {
            (
                format!("{trimmed}{OPENAI_V1_PATH}"),
                FoundrySurface::OpenAIV1,
            )
        }
    }

    /// Audience for an endpoint, before any `entra_scope` override.
    pub fn default_scope(base: &str) -> &'static str {
        if base.contains(PROJECTS_SEGMENT) {
            FOUNDRY_PROJECT_SCOPE
        } else {
            COGNITIVE_SERVICES_SCOPE
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn surface(&self) -> FoundrySurface {
        self.surface
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn api_version(&self) -> Option<&str> {
        self.api_version.as_deref()
    }

    fn with_api_version(&self, url: String) -> String {
        match &self.api_version {
            Some(v) => format!("{url}?api-version={v}"),
            None => url,
        }
    }

    pub fn chat_url(&self) -> String {
        self.with_api_version(format!("{}/chat/completions", self.base))
    }

    pub fn embeddings_url(&self) -> String {
        self.with_api_version(format!("{}/embeddings", self.base))
    }

    /// Model listing for catalog discovery.
    ///
    /// The Model Inference surface has no listing operation at all — only
    /// `/info`, which describes the single deployment behind a serverless
    /// endpoint. The same resource always serves the OpenAI-compatible surface
    /// as well ("the base URL accepts both `https://<resource>.openai.azure.com/openai/v1/`
    /// and `https://<resource>.services.ai.azure.com/openai/v1/`"), and its
    /// `GET /models` lists this resource's deployments, so catalog discovery
    /// swaps surfaces rather than reporting no catalog.
    pub fn models_url(&self) -> String {
        let base = match self.surface {
            FoundrySurface::OpenAIV1 => self.base.clone(),
            FoundrySurface::ModelInference => format!(
                "{}{OPENAI_V1_PATH}",
                self.base.trim_end_matches(MODEL_INFERENCE_PATH).trim_end_matches('/')
            ),
        };
        // `api-version` is optional on this surface; omit it so a version
        // pinned for the inference surface cannot break discovery.
        format!("{base}/models")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(endpoint: &str) -> ProviderConfig {
        ProviderConfig {
            foundry_endpoint: Some(endpoint.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn bare_host_gets_the_openai_compatible_surface() {
        let e = FoundryEndpoint::from_config(&config(
            "https://res.services.ai.azure.com/",
        ))
        .unwrap();
        assert_eq!(e.surface(), FoundrySurface::OpenAIV1);
        assert_eq!(
            e.chat_url(),
            "https://res.services.ai.azure.com/openai/v1/chat/completions"
        );
        // Version-less: the spec defaults api-version to `v1`.
        assert_eq!(e.api_version(), None);
    }

    #[test]
    fn a_models_path_selects_model_inference_and_pins_a_required_api_version() {
        let e =
            FoundryEndpoint::from_config(&config("https://res.services.ai.azure.com/models"))
                .unwrap();
        assert_eq!(e.surface(), FoundrySurface::ModelInference);
        assert_eq!(
            e.chat_url(),
            format!(
                "https://res.services.ai.azure.com/models/chat/completions?api-version={}",
                DEFAULT_MODEL_INFERENCE_API_VERSION
            )
        );
        assert_eq!(
            e.embeddings_url(),
            format!(
                "https://res.services.ai.azure.com/models/embeddings?api-version={}",
                DEFAULT_MODEL_INFERENCE_API_VERSION
            )
        );
    }

    #[test]
    fn an_explicit_api_version_wins_on_either_surface() {
        let mut c = config("https://res.services.ai.azure.com");
        c.api_version = Some("preview".into());
        let e = FoundryEndpoint::from_config(&c).unwrap();
        assert_eq!(
            e.chat_url(),
            "https://res.services.ai.azure.com/openai/v1/chat/completions?api-version=preview"
        );
    }

    #[test]
    fn a_configured_openai_v1_path_is_not_doubled() {
        let e = FoundryEndpoint::from_config(&config(
            "https://res.openai.azure.com/openai/v1",
        ))
        .unwrap();
        assert_eq!(
            e.chat_url(),
            "https://res.openai.azure.com/openai/v1/chat/completions"
        );
    }

    #[test]
    fn project_is_appended_and_switches_the_audience() {
        let mut c = config("https://res.services.ai.azure.com");
        c.project = Some("my-project".into());
        let e = FoundryEndpoint::from_config(&c).unwrap();
        assert_eq!(
            e.chat_url(),
            "https://res.services.ai.azure.com/api/projects/my-project/openai/v1/chat/completions"
        );
        // Project endpoints take the ai.azure.com audience, resource-level
        // inference endpoints take cognitiveservices.
        assert_eq!(e.scope(), FOUNDRY_PROJECT_SCOPE);
    }

    #[test]
    fn a_project_already_in_the_endpoint_is_not_appended_twice() {
        let mut c = config("https://res.services.ai.azure.com/api/projects/p");
        c.project = Some("p".into());
        let e = FoundryEndpoint::from_config(&c).unwrap();
        assert_eq!(
            e.base(),
            "https://res.services.ai.azure.com/api/projects/p/openai/v1"
        );
    }

    #[test]
    fn resource_endpoints_default_to_the_cognitiveservices_audience() {
        let e = FoundryEndpoint::from_config(&config("https://res.services.ai.azure.com"))
            .unwrap();
        assert_eq!(e.scope(), COGNITIVE_SERVICES_SCOPE);
    }

    #[test]
    fn entra_scope_overrides_the_derived_audience() {
        // The published spec and the published how-to disagree about which
        // audience a resource endpoint takes; the operator gets the last word.
        let mut c = config("https://res.services.ai.azure.com");
        c.entra_scope = Some(FOUNDRY_PROJECT_SCOPE.to_string());
        assert_eq!(
            FoundryEndpoint::from_config(&c).unwrap().scope(),
            FOUNDRY_PROJECT_SCOPE
        );
    }

    #[test]
    fn catalog_uses_the_openai_v1_listing_on_both_surfaces() {
        let v1 = FoundryEndpoint::from_config(&config("https://res.services.ai.azure.com"))
            .unwrap();
        assert_eq!(
            v1.models_url(),
            "https://res.services.ai.azure.com/openai/v1/models"
        );
        let mi =
            FoundryEndpoint::from_config(&config("https://res.services.ai.azure.com/models"))
                .unwrap();
        assert_eq!(
            mi.models_url(),
            "https://res.services.ai.azure.com/openai/v1/models"
        );
    }

    #[test]
    fn a_missing_endpoint_names_the_field_and_shows_the_shape() {
        let err = FoundryEndpoint::from_config(&ProviderConfig::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("foundry_endpoint"), "{err}");
        assert!(err.contains("[providers.foundry]"), "{err}");
        assert!(err.contains("services.ai.azure.com"), "{err}");
    }

    #[test]
    fn a_blank_endpoint_is_treated_as_missing() {
        let err = FoundryEndpoint::from_config(&config("   "))
            .unwrap_err()
            .to_string();
        assert!(err.contains("foundry_endpoint"), "{err}");
    }
}
