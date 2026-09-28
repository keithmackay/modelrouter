//! How a `foundry` request authenticates.
//!
//! Two modes, exactly as `bing_grounding` has them, because they are the two
//! the endpoints accept: both published OpenAPI documents list an `api-key`
//! header scheme alongside the OAuth2/Bearer scheme.
//!
//! Entra is the intended mode and the default. Key auth is opt-in and nothing
//! but opt-in: it engages only when `api_key` is non-empty in the provider
//! table, which means an operator wrote a secret to disk deliberately. The
//! Entra credential is chosen by `credential_source` — see
//! `providers::azure_credentials`.

pub use crate::providers::azure_credentials::AzureAuth as FoundryAuth;
