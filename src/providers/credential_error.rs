//! A provider credential that cannot be refreshed without a human.
//!
//! Distinct from every other provider failure because retrying cannot fix it:
//! an OAuth refresh token that the identity provider now refuses
//! (`invalid_grant`, a reauthentication-required policy, a deleted
//! service-account key) stays refused until someone logs in again or changes
//! the configured credential. Treating it like a transient upstream failure is
//! actively harmful — each failure counts toward the provider circuit breaker,
//! the caller then sees "circuit breaker open", which reads as "try again
//! later", and well-behaved clients retry for hours against a fault that will
//! never clear on its own.
//!
//! So this error is typed rather than recognised from a string: the retry loop
//! and the circuit breaker check for it explicitly and step aside, and the API
//! layer maps it to HTTP 401 with the stable code [`CREDENTIAL_EXPIRED_CODE`].
//! Not feature-gated: those consumers are compiled whether or not any provider
//! that can raise it is.

/// Stable machine-readable code for [`CredentialExpired`], used as both the
/// `error.type` and `error.code` fields of the HTTP response body.
pub const CREDENTIAL_EXPIRED_CODE: &str = "credential_expired";

#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "{code}: the {provider} credential ({credential_kind}) can no longer be refreshed and needs \
     human action. {hint} Upstream said: {detail}",
    code = CREDENTIAL_EXPIRED_CODE
)]
pub struct CredentialExpired {
    /// Provider name as configured, e.g. `vertex`.
    pub provider: String,
    /// Which credential failed, e.g. `adc-user` (never the credential itself).
    pub credential_kind: String,
    /// What the operator should do about it.
    pub hint: String,
    /// The identity provider's own error text (error code and description;
    /// never token material).
    pub detail: String,
}

/// Find a [`CredentialExpired`] anywhere in an error's chain, so a call site
/// that added `.context(..)` on top of it does not hide it.
pub fn find_credential_expired(err: &anyhow::Error) -> Option<&CredentialExpired> {
    err.downcast_ref::<CredentialExpired>()
        .or_else(|| err.chain().find_map(|e| e.downcast_ref::<CredentialExpired>()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    fn sample() -> CredentialExpired {
        CredentialExpired {
            provider: "vertex".into(),
            credential_kind: "adc-user".into(),
            hint: "Reauthenticate.".into(),
            detail: "invalid_grant".into(),
        }
    }

    #[test]
    fn found_when_it_is_the_error_itself() {
        let err = anyhow::Error::new(sample());
        assert!(find_credential_expired(&err).is_some());
    }

    #[test]
    fn found_under_layers_of_context() {
        let err: anyhow::Result<()> = Err(anyhow::Error::new(sample()));
        let err = err
            .context("credential rebuild after a 401 also failed")
            .context("outer")
            .unwrap_err();
        let found = find_credential_expired(&err).expect("must survive context wrapping");
        assert_eq!(found.credential_kind, "adc-user");
    }

    #[test]
    fn not_found_for_other_errors() {
        let err = anyhow::anyhow!("Vertex AI returned 503 Service Unavailable");
        assert!(find_credential_expired(&err).is_none());
    }

    #[test]
    fn message_leads_with_the_stable_code() {
        let msg = sample().to_string();
        assert!(msg.starts_with("credential_expired: "), "{msg}");
        assert!(msg.contains("Reauthenticate."), "{msg}");
    }
}
