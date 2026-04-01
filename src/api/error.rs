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
    #[error("provider error: {0}")]
    ProviderError(anyhow::Error),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("policy denied: {reason}")]
    PolicyDenied { reason: String, status: u16 },
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

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::ProviderError(e)
    }
}
