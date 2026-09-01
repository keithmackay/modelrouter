use crate::config::schema::RetryConfig;

#[derive(Debug)]
pub enum RetryableError {
    RateLimit,
    ServerError(u16),
    AuthError,
    /// The provider rejected the request itself — a malformed body, an
    /// unsupported parameter, an unknown model. Distinguished from
    /// [`Self::NotRetryable`] because it says nothing about provider health.
    ClientError(u16),
    NotRetryable,
}

impl RetryableError {
    pub fn classify(err_str: &str) -> Self {
        // Known limitation: providers embed status codes in error strings.
        // A future phase should add typed ProviderError variants.
        if err_str.contains("429") || err_str.to_lowercase().contains("rate limit") {
            Self::RateLimit
        } else if err_str.contains("500") || err_str.contains("502") || err_str.contains("503") {
            Self::ServerError(500)
        } else if err_str.contains("401") || err_str.contains("403") {
            Self::AuthError
        } else if let Some(status) = client_error_status(err_str) {
            Self::ClientError(status)
        } else {
            Self::NotRetryable
        }
    }

    /// Whether this failure is evidence the *provider* is unhealthy, and so
    /// should count toward opening its circuit breaker.
    ///
    /// A client error is not. The breaker exists to stop sending traffic to a
    /// provider that cannot serve it; a 400 means this provider served the
    /// request fine and is telling us the request was wrong. Counting those
    /// lets one caller's bad parameter open a **provider-wide** breaker and
    /// deny every other model behind that provider — observed in production
    /// when a `temperature` a Claude 5 model no longer accepts took down the
    /// entire Vertex provider, including models that would have answered.
    pub fn counts_toward_circuit_breaker(&self) -> bool {
        !matches!(self, Self::ClientError(_))
    }
}

/// Extract a 4xx status the provider reported for the request itself.
///
/// Deliberately narrow. Provider adapters format failures as
/// `"<Provider> returned <status> <reason>: <body>"`, so the status is anchored
/// on `returned ` rather than matched as a bare substring — an error whose
/// echoed request body contains `"max_tokens":4000` must not read as a 400.
/// Anything not positively identified stays [`RetryableError::NotRetryable`]
/// and keeps counting toward the breaker: failing to suppress a client error
/// costs availability, but suppressing a real outage costs correctness.
fn client_error_status(err_str: &str) -> Option<u16> {
    let mut rest = err_str;
    while let Some(pos) = rest.find("returned ") {
        rest = &rest[pos + "returned ".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(status) = digits.parse::<u16>() {
            // 429 is a client status but a genuine health signal, and is
            // already classified above; exclude it defensively.
            if (400..500).contains(&status) && status != 429 {
                return Some(status);
            }
        }
    }
    None
}

pub struct RetryPolicy {
    max_retries: u32,
    base_delay_ms: u64,
    max_delay_ms: u64,
}

impl RetryPolicy {
    pub fn new(max_retries: u32, base_delay_ms: u64, max_delay_ms: u64) -> Self {
        Self { max_retries, base_delay_ms, max_delay_ms }
    }

    pub fn from_config(config: &RetryConfig) -> Self {
        Self::new(config.max_retries, config.base_delay_ms, config.max_delay_ms)
    }

    pub fn should_retry(&self, attempt: u32, error: &RetryableError) -> bool {
        if attempt >= self.max_retries {
            return false;
        }
        matches!(error, RetryableError::RateLimit | RetryableError::ServerError(_))
    }

    pub fn delay_ms(&self, attempt: u32) -> u64 {
        let base = self.base_delay_ms.saturating_mul(2u64.saturating_pow(attempt));
        let capped = base.min(self.max_delay_ms);
        let jitter_range = (capped / 10).max(1);
        let jitter = rand::random::<u64>() % (jitter_range * 2);
        capped.saturating_sub(jitter_range).saturating_add(jitter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact error that opened the Vertex breaker in production.
    const VERTEX_TEMPERATURE_400: &str = concat!(
        "Vertex AI returned 400 Bad Request: ",
        r#"{"type":"error","error":{"type":"invalid_request_error","#,
        r#""message":"`temperature` is deprecated for this model."}}"#,
    );

    #[test]
    fn provider_400_is_a_client_error_and_spares_the_breaker() {
        let classified = RetryableError::classify(VERTEX_TEMPERATURE_400);
        assert!(matches!(classified, RetryableError::ClientError(400)));
        assert!(!classified.counts_toward_circuit_breaker());
    }

    #[test]
    fn client_errors_are_still_not_retried() {
        let policy = RetryPolicy::new(3, 10, 100);
        assert!(!policy.should_retry(0, &RetryableError::classify(VERTEX_TEMPERATURE_400)));
    }

    #[test]
    fn genuine_provider_failures_still_open_the_breaker() {
        for err in [
            "Vertex AI returned 503 Service Unavailable: upstream overloaded",
            "OpenAI returned 429 Too Many Requests",
            "Anthropic returned 401 Unauthorized",
            "connection reset by peer",
        ] {
            assert!(
                RetryableError::classify(err).counts_toward_circuit_breaker(),
                "{err} must still count toward the breaker"
            );
        }
    }

    /// The reason the status is anchored on `returned ` instead of matched as a
    /// bare substring: provider errors echo the request body back.
    #[test]
    fn a_status_like_number_in_the_echoed_body_is_not_a_status() {
        let err = r#"Vertex AI returned 503 Service Unavailable: {"max_tokens":4000,"top_k":404}"#;
        assert!(matches!(
            RetryableError::classify(err),
            RetryableError::ServerError(_)
        ));

        let err = r#"provider error: stream closed unexpectedly {"max_tokens":4000}"#;
        let classified = RetryableError::classify(err);
        assert!(matches!(classified, RetryableError::NotRetryable));
        assert!(classified.counts_toward_circuit_breaker());
    }

    #[test]
    fn other_4xx_request_faults_are_client_errors() {
        for (err, want) in [
            ("OpenAI returned 404 Not Found: unknown model", 404u16),
            ("OpenAI returned 422 Unprocessable Entity", 422),
            ("Vertex AI returned 413 Payload Too Large", 413),
        ] {
            match RetryableError::classify(err) {
                RetryableError::ClientError(got) => assert_eq!(got, want, "{err}"),
                other => panic!("{err} classified as {other:?}"),
            }
        }
    }

    /// 429 is a 4xx but is a real health signal, and must keep its own
    /// classification rather than being swallowed as a client error.
    #[test]
    fn rate_limit_is_not_downgraded_to_a_client_error() {
        let classified = RetryableError::classify("OpenAI returned 429 Too Many Requests");
        assert!(matches!(classified, RetryableError::RateLimit));
        assert!(classified.counts_toward_circuit_breaker());
    }
}
