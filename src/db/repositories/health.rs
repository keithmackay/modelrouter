use async_trait::async_trait;

/// Liveness of the connection to the store behind a backend. A readiness
/// probe calls this on every poll, so an implementation must be the cheapest
/// round trip the backend offers.
#[async_trait]
pub trait HealthRepository: Send + Sync {
    async fn ping(&self) -> anyhow::Result<()>;
}
