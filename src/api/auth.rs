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
        use crate::db::repositories::api_keys::ApiKeyRepository;
        use crate::db::repositories::users::UserRepository;

        let auth_header = parts
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or(ApiError::Unauthorized)?;

        let key_hash = hash_token(auth_header);

        // 1. Try api_keys table first
        if let Some(api_key) = ApiKeyRepository::find_api_key_by_hash(&*state.db, &key_hash)
            .await
            .map_err(|_| ApiError::Internal)?
        {
            let mut user = UserRepository::find_by_id(&*state.db, api_key.user_id)
                .await
                .map_err(|_| ApiError::Internal)?
                .ok_or(ApiError::Unauthorized)?;

            if !user.enabled {
                return Err(ApiError::Unauthorized);
            }
            user.api_key_id = Some(api_key.id);
            return Ok(AuthenticatedUser(user));
        }

        // 2. Fall back to legacy users.api_key
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
