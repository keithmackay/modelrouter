use crate::config::schema::RetryConfig;

#[derive(Debug)]
pub enum RetryableError {
    RateLimit,
    ServerError(u16),
    AuthError,
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
        } else {
            Self::NotRetryable
        }
    }
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
