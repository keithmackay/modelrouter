use async_trait::async_trait;

#[async_trait]
pub trait RateLimitRepository: Send + Sync {
    /// Atomically increment the count and return the new value.
    async fn increment_and_get_count(&self, user_id: i64, window_key: &str) -> anyhow::Result<i64>;
    /// Delete rate limit entries with window_key older than the given cutoff string.
    async fn cleanup_old_windows(&self, before_window_key: &str) -> anyhow::Result<u64>;
}
