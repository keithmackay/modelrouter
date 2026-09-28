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
        // A credential that needs a human is not a gateway fault: 401 with a
        // stable code tells the caller "stop retrying, go fix auth" instead of
        // the 502 that invites a retry loop. The body carries the router's own
        // provider-neutral reason and remediation, so a caller can act on it
        // without knowing which provider or platform is behind the router.
        if let ApiError::ProviderError(e) = &self {
            if let Some(expired) = crate::providers::credential_error::find_credential_expired(e) {
                let code = crate::providers::credential_error::CREDENTIAL_EXPIRED_CODE;
                let body = json!({
                    "error": {
                        "message": expired.to_string(),
                        "type": code,
                        "code": code,
                        "provider": expired.provider,
                        "credential_kind": expired.credential_kind,
                        "reason": expired.reason,
                        "remediation": expired.remediation,
                    }
                });
                return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::credential_error::CredentialExpired;
    use anyhow::Context;

    async fn status_and_body(err: ApiError) -> (StatusCode, serde_json::Value) {
        let resp = err.into_response();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn credential_expired_maps_to_401_with_a_stable_code() {
        let inner: anyhow::Result<()> = Err(anyhow::Error::new(CredentialExpired {
            provider: "vertex".into(),
            credential_kind: "adc-user".into(),
            reason: "the login has expired".into(),
            remediation: "Reauthenticate: gcloud auth application-default login.".into(),
            detail: "invalid_grant".into(),
        }));
        let err = inner.context("failed to send request").unwrap_err();
        let (status, body) = status_and_body(ApiError::ProviderError(err)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "credential_expired");
        assert_eq!(body["error"]["type"], "credential_expired");
        assert_eq!(body["error"]["provider"], "vertex");
        assert_eq!(body["error"]["credential_kind"], "adc-user");
        assert_eq!(body["error"]["reason"], "the login has expired");
        assert_eq!(
            body["error"]["remediation"],
            "Reauthenticate: gcloud auth application-default login."
        );
        let msg = body["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("gcloud auth application-default login"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn other_provider_errors_stay_502() {
        let err = anyhow::anyhow!("Vertex AI returned 503 Service Unavailable");
        let (status, body) = status_and_body(ApiError::ProviderError(err)).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"]["code"], "provider_error");
    }
}
