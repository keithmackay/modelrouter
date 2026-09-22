use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("provider error: {0}")]
    ProviderError(anyhow::Error),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("policy denied: {reason}")]
    PolicyDenied { reason: String, status: u16 },
    /// A model or provider an operator has taken out of rotation (issue #5).
    /// Deliberately *not* a `ProviderError`: nothing was called upstream.
    #[error("{0}")]
    Disabled(String),
    #[error("internal error")]
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message, code) = match &self {
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized".to_string(),
                "auth_error",
            ),
            ApiError::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden".to_string(),
                "forbidden",
            ),
            ApiError::ProviderError(e) => {
                (StatusCode::BAD_GATEWAY, e.to_string(), "provider_error")
            }
            ApiError::InvalidRequest(msg) => {
                (StatusCode::BAD_REQUEST, msg.clone(), "invalid_request")
            }
            ApiError::PolicyDenied { reason, status } => {
                let sc = StatusCode::from_u16(*status)
                    .unwrap_or(StatusCode::TOO_MANY_REQUESTS);
                (sc, reason.clone(), "policy_denied")
            }
            ApiError::Disabled(msg) => (
                StatusCode::FORBIDDEN,
                msg.clone(),
                "model_disabled",
            ),
            ApiError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
                "internal_error",
            ),
        };
        let body = json!({
            "error": {
                "message": message,
                "type": code,
                "code": code,
            }
        });
        (status, Json(body)).into_response()
    }
}

impl From<crate::router::availability::Unavailable> for ApiError {
    fn from(u: crate::router::availability::Unavailable) -> Self {
        ApiError::Disabled(u.message())
    }
}

/// A request that named an experiment it cannot bind to. Always the caller's
/// fault (bad header, unknown id, not on the allow list), hence 400.
impl From<crate::router::experiments::BindError> for ApiError {
    fn from(e: crate::router::experiments::BindError) -> Self {
        ApiError::InvalidRequest(e.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::ProviderError(e)
    }
}
