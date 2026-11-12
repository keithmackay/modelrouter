// tests/test_concurrency.rs

#[tokio::test]
async fn limiter_allows_requests_under_limit() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    assert!(limiter.try_acquire(1, 2).await.is_some(), "first should succeed");
    assert!(limiter.try_acquire(1, 2).await.is_some(), "second should succeed");
}

#[tokio::test]
async fn limiter_denies_when_at_capacity() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    let _p1 = limiter.try_acquire(1, 1).await.expect("first should succeed");
    assert!(limiter.try_acquire(1, 1).await.is_none(), "second should be denied");
}

#[tokio::test]
async fn permit_drop_releases_slot() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    { let _p = limiter.try_acquire(1, 1).await.expect("first"); }
    assert!(limiter.try_acquire(1, 1).await.is_some(), "slot available after drop");
}

#[tokio::test]
async fn users_tracked_independently() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    let _p1 = limiter.try_acquire(1, 1).await.expect("user 1");
    assert!(limiter.try_acquire(2, 1).await.is_some(), "user 2 is independent");
}

#[tokio::test]
async fn max_zero_denies_all() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    assert!(limiter.try_acquire(1, 0).await.is_none());
}
