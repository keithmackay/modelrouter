//! Provider model-catalog discovery (issue #23 / #32).
//!
//! A `ProviderCatalog` lists the models a connected provider actually offers,
//! using the same credentials the request-path adapter already holds. The
//! aggregation endpoint (#34) fans out over these; the mapping UI (#35)
//! consumes the merged result so operators pick real models instead of
//! typing free-form IDs.

use async_trait::async_trait;

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
}

/// Catalog discovery capability. Implemented per provider adapter; providers
/// whose API has no catalog surface simply do not implement it (#34 treats
/// absence as "not supported", not as an empty catalog).
#[async_trait]
pub trait ProviderCatalog: Send + Sync {
    async fn list_models(&self) -> anyhow::Result<Vec<CatalogModel>>;
}
