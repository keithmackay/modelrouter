// tests/test_concurrency.rs

#[test]
fn limiter_allows_requests_under_limit() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    assert!(limiter.try_acquire(1, 2).is_some(), "first should succeed");
    assert!(limiter.try_acquire(1, 2).is_some(), "second should succeed");
}

#[test]
fn limiter_denies_when_at_capacity() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    let _p1 = limiter.try_acquire(1, 1).expect("first should succeed");
    assert!(limiter.try_acquire(1, 1).is_none(), "second should be denied");
}

#[test]
fn permit_drop_releases_slot() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    { let _p = limiter.try_acquire(1, 1).expect("first"); }
    assert!(limiter.try_acquire(1, 1).is_some(), "slot available after drop");
}

#[test]
fn users_tracked_independently() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    let _p1 = limiter.try_acquire(1, 1).expect("user 1");
    assert!(limiter.try_acquire(2, 1).is_some(), "user 2 is independent");
}

#[test]
fn max_zero_denies_all() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    assert!(limiter.try_acquire(1, 0).is_none());
}
