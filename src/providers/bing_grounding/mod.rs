//! Grounding with Bing Search on Azure AI Foundry (--features bing-grounding).
//!
//! Search only: this provider has no chat/embedding adapter, because the thing
//! being integrated is a Foundry *tool*, not a model family.
pub mod auth;
pub mod search;
pub use search::BingGroundingAdapter;
