// tests/test_ip_rate_limit.rs
use modelrouter::api::middleware::ip_rate_limit::IpRateLimiter;

#[test]
fn allows_requests_under_limit() {
    let limiter = IpRateLimiter::new(3);
    assert!(limiter.check_and_increment("1.2.3.4"));
    assert!(limiter.check_and_increment("1.2.3.4"));
    assert!(limiter.check_and_increment("1.2.3.4"));
}

#[test]
fn denies_requests_over_limit() {
    let limiter = IpRateLimiter::new(2);
    assert!(limiter.check_and_increment("1.2.3.4"));
    assert!(limiter.check_and_increment("1.2.3.4"));
    assert!(!limiter.check_and_increment("1.2.3.4"), "third request should be denied");
}

#[test]
fn ips_tracked_independently() {
    let limiter = IpRateLimiter::new(1);
    assert!(limiter.check_and_increment("1.2.3.4"));
    assert!(!limiter.check_and_increment("1.2.3.4"), "1.2.3.4 at limit");
    assert!(limiter.check_and_increment("5.6.7.8"), "5.6.7.8 unaffected");
}

#[test]
fn zero_limit_disables_limiting() {
    let limiter = IpRateLimiter::new(0);
    for _ in 0..100 {
        assert!(limiter.check_and_increment("1.2.3.4"));
    }
}
