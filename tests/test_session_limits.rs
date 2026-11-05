use modelrouter::router::session_limits::SessionLimiter;

#[tokio::test]
async fn allows_first_request() {
    let limiter = SessionLimiter::new(10000, 10);
    assert!(limiter.check_and_record("session-abc", 100).await);
}

#[tokio::test]
async fn blocks_after_rpm_exceeded() {
    let limiter = SessionLimiter::new(10000, 2);
    assert!(limiter.check_and_record("s", 10).await);
    assert!(limiter.check_and_record("s", 10).await);
    assert!(!limiter.check_and_record("s", 10).await); // 3rd blocked
}

#[tokio::test]
async fn blocks_after_tpm_exceeded() {
    let limiter = SessionLimiter::new(50, 1000);
    assert!(limiter.check_and_record("s", 40).await);
    assert!(!limiter.check_and_record("s", 20).await); // 40+20 > 50
}

#[tokio::test]
async fn different_sessions_independent() {
    let limiter = SessionLimiter::new(10000, 1);
    assert!(limiter.check_and_record("a", 10).await);
    assert!(limiter.check_and_record("b", 10).await);
}

#[tokio::test]
async fn zero_limits_always_allow() {
    let limiter = SessionLimiter::new(0, 0);
    for _ in 0..100 {
        assert!(limiter.check_and_record("x", 9999).await);
    }
}
