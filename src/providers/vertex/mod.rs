//! GCP Vertex AI provider (--features vertex).
pub mod auth;
pub mod dispatch;
pub mod gemini;
pub mod claude;
pub mod maas;
pub mod adapter;
pub mod catalog;
pub mod embed;
pub mod search;
pub use adapter::VertexAdapter;
pub use embed::VertexEmbeddingAdapter;
pub use search::VertexSearchAdapter;
