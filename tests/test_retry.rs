use modelrouter::router::retry::{RetryPolicy, RetryableError};

#[test]
fn allows_up_to_max_retries() {
    let policy = RetryPolicy::new(3, 100, 1000);
    assert!(policy.should_retry(0, &RetryableError::RateLimit));
    assert!(policy.should_retry(2, &RetryableError::RateLimit));
    assert!(!policy.should_retry(3, &RetryableError::RateLimit));
}

#[test]
fn does_not_retry_auth_errors() {
    let policy = RetryPolicy::new(3, 100, 1000);
    assert!(!policy.should_retry(0, &RetryableError::AuthError));
    assert!(!policy.should_retry(0, &RetryableError::NotRetryable));
}

#[test]
fn delay_increases_with_attempt_number() {
    let policy = RetryPolicy::new(5, 100, 100_000);
    let d0 = policy.delay_ms(0);
    let d1 = policy.delay_ms(1);
    let d2 = policy.delay_ms(2);
    assert!(d1 >= d0, "d1={d1} should be >= d0={d0}");
    assert!(d2 >= d1, "d2={d2} should be >= d1={d1}");
}

#[test]
fn delay_capped_at_max() {
    let policy = RetryPolicy::new(10, 1000, 5000);
    for attempt in 5..10 {
        assert!(policy.delay_ms(attempt) <= 5500, "exceeded max at attempt {attempt}");
    }
}

#[test]
fn classify_rate_limit() {
    assert!(matches!(RetryableError::classify("status: 429"), RetryableError::RateLimit));
    assert!(matches!(RetryableError::classify("rate limit exceeded"), RetryableError::RateLimit));
}

#[test]
fn classify_server_error() {
    assert!(matches!(RetryableError::classify("status 503"), RetryableError::ServerError(_)));
}

#[test]
fn classify_not_retryable() {
    assert!(matches!(RetryableError::classify("invalid json"), RetryableError::NotRetryable));
}
