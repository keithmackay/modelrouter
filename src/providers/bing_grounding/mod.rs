//! Grounding with Bing Search on Azure AI Foundry (--features bing-grounding).
//!
//! Search only: this provider has no chat/embedding adapter, because the thing
//! being integrated is a Foundry *tool*, not a model family. Chat completions
//! and embeddings against Foundry-hosted models are the `foundry` provider.
//!
//! The Entra token source this adapter uses now lives in
//! `crate::providers::azure_entra`. It was promoted out of here unchanged —
//! bar a per-caller `scope` argument — when `foundry` needed the same
//! credential chain for a different audience.
pub mod search;
pub use search::BingGroundingAdapter;
