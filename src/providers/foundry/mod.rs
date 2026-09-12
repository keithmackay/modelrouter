//! Azure AI Foundry provider (`--features foundry`, on by default).
//!
//! Routes `foundry/<deployment>` to models deployed in an Azure AI Foundry
//! (AI Services) resource, the way `vertex` routes to models in a GCP project:
//! chat completions, streaming, embeddings and catalog discovery, all
//! authenticating with the Microsoft Entra credential chain rather than a key
//! written to config.
//!
//! Distinct from `providers::azure_openai`, which speaks the older
//! `/openai/deployments/{deployment}/...?api-version=` Azure OpenAI surface
//! with an `api-key` header. This provider speaks the Foundry data planes
//! (`/openai/v1` or `/models`) and defaults to keyless auth.
//!
//! - `endpoint` — endpoint/surface/api-version/scope resolution
//! - `auth` — Entra by default, `api-key` header on explicit opt-in
//! - `adapter` — chat completions and streaming
//! - `embed` — embeddings
//! - `catalog` — deployed-model discovery
//!
//! The Entra token source is shared with `bing_grounding` and lives in
//! `crate::providers::azure_entra`; the audience is passed per caller.

pub mod adapter;
pub mod auth;
pub mod catalog;
pub mod embed;
pub mod endpoint;

pub use adapter::FoundryAdapter;
pub use embed::FoundryEmbeddingAdapter;
pub use endpoint::{FoundryEndpoint, FoundrySurface};
