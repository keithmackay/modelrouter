//! GCP Vertex AI provider (--features vertex).
pub mod auth;
pub mod dispatch;
pub mod gemini;
pub mod claude;
pub mod adapter;
pub use adapter::VertexAdapter;
