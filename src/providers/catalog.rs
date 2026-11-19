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

/// Name fragments that mark a model as something other than chat, checked in
/// this order against a lower-cased id. One list for every provider, so a
/// configured deployment, a discovered one and a plain `/models` listing all
/// read the same name the same way.
const NAME_KINDS: &[(&[&str], CatalogKind)] = &[
    (&["embed"], CatalogKind::Embedding),
    (&["rerank"], CatalogKind::Rerank),
    (&["whisper", "tts", "transcribe", "realtime", "speech"], CatalogKind::Audio),
    (&["dall-e", "dalle", "image", "flux", "sora", "stable-diffusion"], CatalogKind::Image),
];

impl CatalogKind {
    /// What a model can answer, judged from its id alone: the only evidence a
    /// `/models` listing or a configured deployment name carries. A name that
    /// says nothing is chat: that is what a model is for unless something marks
    /// it otherwise, and a wrong guess costs one refused call, not a missing
    /// model. An embedding, reranking, image or audio model offered for chat
    /// fails on its first call, so those names are never read as chat.
    pub fn from_name(name: &str) -> CatalogKind {
        let lower = name.to_ascii_lowercase();
        NAME_KINDS
            .iter()
            .find(|(needles, _)| needles.iter().any(|n| lower.contains(n)))
            .map(|(_, kind)| *kind)
            .unwrap_or(CatalogKind::Chat)
    }

    /// [`Self::from_name`] over several names for one model (a deployment and
    /// the base model behind it): the first that says more than chat wins.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> CatalogKind {
        names
            .into_iter()
            .map(CatalogKind::from_name)
            .find(|k| *k != CatalogKind::Chat)
            .unwrap_or(CatalogKind::Chat)
    }
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

#[cfg(test)]
mod tests {
    use super::CatalogKind;

    #[test]
    fn a_name_that_says_what_it_serves_is_never_read_as_chat() {
        assert_eq!(CatalogKind::from_name("text-embedding-3-large"), CatalogKind::Embedding);
        assert_eq!(CatalogKind::from_name("Cohere-embed-v4"), CatalogKind::Embedding);
        assert_eq!(CatalogKind::from_name("Cohere-rerank-v4.0-pro"), CatalogKind::Rerank);
        assert_eq!(CatalogKind::from_name("whisper-1"), CatalogKind::Audio);
        assert_eq!(CatalogKind::from_name("gpt-4o-mini-tts"), CatalogKind::Audio);
        assert_eq!(CatalogKind::from_name("gpt-4o-realtime-preview"), CatalogKind::Audio);
        assert_eq!(CatalogKind::from_name("dall-e-3"), CatalogKind::Image);
        assert_eq!(CatalogKind::from_name("gpt-image-1"), CatalogKind::Image);
        assert_eq!(CatalogKind::from_name("gpt-4.1"), CatalogKind::Chat);
        assert_eq!(CatalogKind::from_name("claude-sonnet-4-5"), CatalogKind::Chat);
        assert_eq!(CatalogKind::from_name("DeepSeek-V4-Pro"), CatalogKind::Chat);
    }

    #[test]
    fn several_names_for_one_model_take_the_first_that_says_more_than_chat() {
        assert_eq!(CatalogKind::from_names(["my-eu-vectors", "text-embedding-3-large"]), CatalogKind::Embedding);
        assert_eq!(CatalogKind::from_names(["ranker", "Cohere-rerank-v4.0-pro"]), CatalogKind::Rerank);
        assert_eq!(CatalogKind::from_names(["my-chat", "gpt-5"]), CatalogKind::Chat);
        assert_eq!(CatalogKind::from_names(std::iter::empty()), CatalogKind::Chat);
    }
}
