//! Per-user concurrency limiter.
//!
//! In memory, one semaphore per user, created on first use with this
//! replica's share of the limit. With a cluster Redis link, one sorted set
//! per user holds every live permit across all replicas, scored by its lease
//! expiry: a permit is refreshed while held and vanishes on its own if the
//! replica holding it dies. When Redis is unreachable the limiter falls back
//! to the in-memory share, so a Redis outage loosens limits to the phase-1
//! level rather than failing requests.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::cluster::RedisLink;
use crate::config::schema::ClusterConfig;

pub struct ConcurrencyLimiter {
    semaphores: DashMap<i64, Arc<Semaphore>>,
    cluster: ClusterConfig,
    redis: Option<Arc<RedisLink>>,
}

/// Held for the duration of the upstream call; dropping it releases the slot.
pub enum ConcurrencyPermit {
    Local(OwnedSemaphorePermit),
    Shared(SharedPermit),
}

/// One member of a user's permit set in Redis, kept alive by a refresh task
/// and removed on drop.
pub struct SharedPermit {
    link: Arc<RedisLink>,
    key: String,
    member: String,
    refresh: tokio::task::JoinHandle<()>,
}

impl Drop for SharedPermit {
    fn drop(&mut self) {
        self.refresh.abort();
        let link = self.link.clone();
        let key = std::mem::take(&mut self.key);
        let member = std::mem::take(&mut self.member);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _: Option<i64> = link
                    .run("zrem", redis::cmd("ZREM").arg(&key).arg(&member))
                    .await;
            });
        }
    }
}

const ACQUIRE_SCRIPT: &str = r#"
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', ARGV[1])
if redis.call('ZCARD', KEYS[1]) >= tonumber(ARGV[2]) then
  return 0
end
redis.call('ZADD', KEYS[1], ARGV[3], ARGV[4])
redis.call('EXPIRE', KEYS[1], ARGV[5])
return 1
"#;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl ConcurrencyLimiter {
    pub fn new() -> Self {
        Self::for_cluster(ClusterConfig::default())
    }

    /// A limiter that enforces this replica's share of each user's limit.
    /// The limit itself arrives per request from the budget rules, so the
    /// division happens on acquire rather than at construction.
    pub fn for_cluster(cluster: ClusterConfig) -> Self {
        Self {
            semaphores: DashMap::new(),
            cluster,
            redis: None,
        }
    }

    /// Count permits across replicas in Redis. The full limit applies there;
    /// the in-memory share is kept as the fallback.
    pub fn with_redis(mut self, link: Option<Arc<RedisLink>>) -> Self {
        self.redis = link;
        self
    }

    /// Try to acquire a slot for `user_id` under a limit of `max` across the
    /// deployment. `None` when at capacity or `max` is 0.
    pub async fn try_acquire(&self, user_id: i64, max: u32) -> Option<ConcurrencyPermit> {
        if max == 0 {
            return None;
        }
        if let Some(link) = &self.redis {
            match self.try_acquire_shared(link, user_id, max).await {
                Ok(Some(permit)) => return Some(ConcurrencyPermit::Shared(permit)),
                Ok(None) => return None,
                Err(()) => {}
            }
        }
        self.try_acquire_local(user_id, self.cluster.share(max))
            .map(ConcurrencyPermit::Local)
    }

    fn try_acquire_local(&self, user_id: i64, max: u32) -> Option<OwnedSemaphorePermit> {
        if max == 0 {
            return None;
        }
        let semaphore = self
            .semaphores
            .entry(user_id)
            .or_insert_with(|| Arc::new(Semaphore::new(max as usize)))
            .clone();
        semaphore.try_acquire_owned().ok()
    }

    /// `Err(())` means Redis did not answer; the caller falls back.
    async fn try_acquire_shared(
        &self,
        link: &Arc<RedisLink>,
        user_id: i64,
        max: u32,
    ) -> Result<Option<SharedPermit>, ()> {
        let lease = Duration::from_secs(self.cluster.lease_secs.max(1));
        let key = link.key(&format!("cc:{user_id}"));
        let member = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let admitted: i64 = link
            .run(
                "concurrency acquire",
                redis::cmd("EVAL")
                    .arg(ACQUIRE_SCRIPT)
                    .arg(1)
                    .arg(&key)
                    .arg(now)
                    .arg(max)
                    .arg(now + lease.as_millis() as u64)
                    .arg(&member)
                    .arg(lease.as_secs() * 2),
            )
            .await
            .ok_or(())?;
        if admitted == 0 {
            return Ok(None);
        }
        let refresh = {
            let link = link.clone();
            let key = key.clone();
            let member = member.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(lease / 3);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    let expiry = now_ms() + lease.as_millis() as u64;
                    let _: Option<i64> = link
                        .run(
                            "concurrency refresh",
                            redis::cmd("ZADD")
                                .arg(&key)
                                .arg("XX")
                                .arg(expiry)
                                .arg(&member),
                        )
                        .await;
                }
            })
        };
        Ok(Some(SharedPermit {
            link: link.clone(),
            key,
            member,
            refresh,
        }))
    }
}

impl Default for ConcurrencyLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn each_replica_gets_its_share_of_the_user_limit() {
        let limiter = ConcurrencyLimiter::for_cluster(ClusterConfig {
            replicas: 2,
            ..ClusterConfig::default()
        });
        let first = limiter.try_acquire(7, 4).await;
        let second = limiter.try_acquire(7, 4).await;
        let third = limiter.try_acquire(7, 4).await;
        assert!(
            first.is_some() && second.is_some(),
            "4 across 2 replicas is 2 here"
        );
        assert!(third.is_none());
    }

    #[tokio::test]
    async fn a_limit_of_one_still_admits_one_on_every_replica() {
        let limiter = ConcurrencyLimiter::for_cluster(ClusterConfig {
            replicas: 5,
            ..ClusterConfig::default()
        });
        let held = limiter.try_acquire(1, 1).await;
        assert!(held.is_some());
        assert!(limiter.try_acquire(1, 1).await.is_none());
    }

    #[tokio::test]
    async fn zero_still_denies_and_one_replica_is_unchanged() {
        let limiter = ConcurrencyLimiter::new();
        assert!(limiter.try_acquire(1, 0).await.is_none());
        let a = limiter.try_acquire(2, 2).await;
        let b = limiter.try_acquire(2, 2).await;
        assert!(a.is_some() && b.is_some());
        assert!(limiter.try_acquire(2, 2).await.is_none());
    }

    #[tokio::test]
    async fn unreachable_redis_falls_back_to_the_local_share() {
        let link = RedisLink::open("redis://127.0.0.1:1", "t").unwrap();
        let limiter = ConcurrencyLimiter::for_cluster(ClusterConfig {
            replicas: 2,
            ..ClusterConfig::default()
        })
        .with_redis(Some(link));
        let a = limiter.try_acquire(1, 2).await;
        assert!(
            matches!(a, Some(ConcurrencyPermit::Local(_))),
            "fell back to the local share"
        );
        assert!(
            limiter.try_acquire(1, 2).await.is_none(),
            "share of 2 across 2 replicas is 1"
        );
    }
}
