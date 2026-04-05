use modelrouter::router::session_limits::SessionLimiter;

#[test]
fn allows_first_request() {
    let limiter = SessionLimiter::new(10000, 10);
    assert!(limiter.check_and_record("session-abc", 100));
}

#[test]
fn blocks_after_rpm_exceeded() {
    let limiter = SessionLimiter::new(10000, 2);
    assert!(limiter.check_and_record("s", 10));
    assert!(limiter.check_and_record("s", 10));
    assert!(!limiter.check_and_record("s", 10)); // 3rd blocked
}

#[test]
fn blocks_after_tpm_exceeded() {
    let limiter = SessionLimiter::new(50, 1000);
    assert!(limiter.check_and_record("s", 40));
    assert!(!limiter.check_and_record("s", 20)); // 40+20 > 50
}

#[test]
fn different_sessions_independent() {
    let limiter = SessionLimiter::new(10000, 1);
    assert!(limiter.check_and_record("a", 10));
    assert!(limiter.check_and_record("b", 10));
}

#[test]
fn zero_limits_always_allow() {
    let limiter = SessionLimiter::new(0, 0);
    for _ in 0..100 {
        assert!(limiter.check_and_record("x", 9999));
    }
}
