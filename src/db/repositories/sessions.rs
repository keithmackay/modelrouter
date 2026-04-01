use async_trait::async_trait;
use crate::db::models::{Session, NewSession};

#[async_trait]
pub trait SessionRepository: Send + Sync {
    async fn find_or_create(&self, session: NewSession) -> anyhow::Result<Session>;
    async fn update_last_seen(&self, id: i64) -> anyhow::Result<()>;
}
