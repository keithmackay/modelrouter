//! Provider model-catalog discovery (issue #23 / #32).
//!
//! A `ProviderCatalog` lists the models a connected provider actually offers,
//! using the same credentials the request-path adapter already holds. The
//! aggregation endpoint (#34) fans out over these; the mapping UI (#35)
//! consumes the merged result so operators pick real models instead of
//! typing free-form IDs.

use async_trait::async_trait;

/// What a catalog entry can answer, so a chat picker never offers a
/// deployment that only serves decisions and a decisions picker lists only
/// those that do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogKind {
    /// Chat completions (and the other chat-shaped surfaces).
    #[default]
    Chat,
    /// `POST /v1/decisions` only (a provider's `decision_deployments`).
    Decision,
    /// Embeddings only (never offered for chat).
    Embedding,
    /// Reranking only.
    Rerank,
    /// Image generation.
    Image,
    /// Speech, transcription or realtime audio.
    Audio,
    /// Deployed, but of no kind the router serves.
    Other,
}

/// Where a catalog entry came from, when the provider can tell: named in its
/// configuration, or found on the account beyond the configured list
/// (`discover_deployments`). Absent for providers whose listing is the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    Configured,
    Discovered,
}

/// One model advertised by a provider's catalog API.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CatalogModel {
    /// Router provider name (e.g. "vertex") — the prefix used in routing ids.
    pub provider: String,
    /// Model id as the router would route it under this provider, e.g.
    /// "anthropic/claude-sonnet-4-5@20250929" for Vertex publisher models.
    pub name: String,
    /// Human-readable name when the catalog supplies one.
    pub display_name: Option<String>,
    /// See [`CatalogKind`]; `chat` unless the provider says otherwise.
    #[serde(default)]
    pub kind: CatalogKind,
    /// See [`CatalogSource`]; omitted when the provider does not distinguish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<CatalogSource>,
}

/// Catalog discovery capability. Implemented per provider adapter; providers
/// whose API has no catalog surface simply do not implement it (#34 treats
/// absence as "not supported", not as an empty catalog).
#[async_trait]
pub trait ProviderCatalog: Send + Sync {
    async fn list_models(&self) -> anyhow::Result<Vec<CatalogModel>>;
}
