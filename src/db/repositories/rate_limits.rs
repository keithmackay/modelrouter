use async_trait::async_trait;

#[async_trait]
pub trait RateLimitRepository: Send + Sync {
    async fn get_request_count(&self, user_id: i64, window_key: &str) -> anyhow::Result<i64>;
    async fn increment_request_count(&self, user_id: i64, window_key: &str) -> anyhow::Result<()>;
}
