use axum::{
    async_trait,
    extract::FromRequestParts,
    http::request::Parts,
};

use crate::{
    api::{app::AppState, error::ApiError},
};

pub fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

#[derive(Debug, Clone)]
pub struct AuthenticatedUser(pub crate::db::models::User);

#[async_trait]
impl FromRequestParts<AppState> for AuthenticatedUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Extract Bearer token
        let auth_header = parts
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or(ApiError::Unauthorized)?;

        let key_hash = hash_token(auth_header);
        let user = state
            .db
            .find_by_api_key(&key_hash)
            .await
            .map_err(|_| ApiError::Internal)?
            .ok_or(ApiError::Unauthorized)?;

        if !user.enabled {
            return Err(ApiError::Unauthorized);
        }
        Ok(AuthenticatedUser(user))
    }
}
