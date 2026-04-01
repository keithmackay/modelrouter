use serde::{Deserialize, Serialize};
use jsonwebtoken::{encode, decode, Header, Algorithm, Validation, EncodingKey, DecodingKey};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AdminClaims {
    pub sub: i64,
    pub name: String,
    pub role: String,
    pub exp: usize,
}

pub fn issue_jwt(claims: &AdminClaims, secret: &str) -> anyhow::Result<String> {
    let token = encode(
        &Header::default(),
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?;
    Ok(token)
}

pub fn verify_jwt(token: &str, secret: &str) -> anyhow::Result<AdminClaims> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    let data = decode::<AdminClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )?;
    Ok(data.claims)
}

#[derive(Debug, Clone)]
pub struct AdminSession(pub AdminClaims);

#[async_trait::async_trait]
impl axum::extract::FromRequestParts<crate::api::app::AppState> for AdminSession {
    type Rejection = crate::api::error::ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &crate::api::app::AppState,
    ) -> Result<Self, Self::Rejection> {
        use crate::api::error::ApiError;

        // Check Authorization: Bearer header first
        let token = if let Some(auth) = parts
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
        {
            auth.to_string()
        } else {
            // Check HttpOnly cookie
            parts
                .headers
                .get("Cookie")
                .and_then(|v| v.to_str().ok())
                .and_then(|cookies| {
                    cookies.split(';').find_map(|c| {
                        let c = c.trim();
                        c.strip_prefix("admin_token=").map(|v| v.to_string())
                    })
                })
                .ok_or(ApiError::Unauthorized)?
        };

        let claims = verify_jwt(&token, &state.settings.auth.jwt_secret)
            .map_err(|_| ApiError::Unauthorized)?;

        Ok(AdminSession(claims))
    }
}

/// Superadmin guard — requires role == "superadmin"
pub struct SuperAdminSession(pub AdminClaims);

#[async_trait::async_trait]
impl axum::extract::FromRequestParts<crate::api::app::AppState> for SuperAdminSession {
    type Rejection = crate::api::error::ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &crate::api::app::AppState,
    ) -> Result<Self, Self::Rejection> {
        use crate::api::error::ApiError;
        let session = AdminSession::from_request_parts(parts, state).await?;
        if session.0.role != "superadmin" {
            return Err(ApiError::Forbidden);
        }
        Ok(SuperAdminSession(session.0))
    }
}
