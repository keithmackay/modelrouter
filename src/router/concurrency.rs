// src/router/concurrency.rs
//
// Per-user concurrency limiter using DashMap<user_id, Arc<Semaphore>>.
// Semaphores are created lazily on first use. The capacity is fixed at the
// value passed on first call for each user_id — if the budget rule changes,
// the new limit takes effect only after a process restart. This is a known
// limitation acceptable for v1.

use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub struct ConcurrencyLimiter {
    semaphores: DashMap<i64, Arc<Semaphore>>,
    cluster: crate::config::schema::ClusterConfig,
}

impl ConcurrencyLimiter {
    pub fn new() -> Self {
        Self::for_cluster(crate::config::schema::ClusterConfig::default())
    }

    /// A limiter that enforces this replica's share of each user's limit.
    /// The limit itself arrives per request from the budget rules, so the
    /// division happens on acquire rather than at construction.
    pub fn for_cluster(cluster: crate::config::schema::ClusterConfig) -> Self {
        Self {
            semaphores: DashMap::new(),
            cluster,
        }
    }

    /// Try to acquire a slot for `user_id` with capacity `max`.
    ///
    /// Returns `Some(permit)` if a slot was available, `None` if at capacity
    /// or `max` is 0. Hold the returned permit for the duration of the upstream
    /// call — dropping it releases the slot.
    pub fn try_acquire(&self, user_id: i64, max: u32) -> Option<OwnedSemaphorePermit> {
        let max = self.cluster.share(max);
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
}

impl Default for ConcurrencyLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::ClusterConfig;

    #[test]
    fn each_replica_gets_its_share_of_the_user_limit() {
        let limiter = ConcurrencyLimiter::for_cluster(ClusterConfig { replicas: 2 });
        let first = limiter.try_acquire(7, 4);
        let second = limiter.try_acquire(7, 4);
        let third = limiter.try_acquire(7, 4);
        assert!(
            first.is_some() && second.is_some(),
            "4 across 2 replicas is 2 here"
        );
        assert!(third.is_none());
    }

    #[test]
    fn a_limit_of_one_still_admits_one_on_every_replica() {
        let limiter = ConcurrencyLimiter::for_cluster(ClusterConfig { replicas: 5 });
        let held = limiter.try_acquire(1, 1);
        assert!(held.is_some());
        assert!(limiter.try_acquire(1, 1).is_none());
    }

    #[test]
    fn zero_still_denies_and_one_replica_is_unchanged() {
        let limiter = ConcurrencyLimiter::new();
        assert!(limiter.try_acquire(1, 0).is_none());
        let a = limiter.try_acquire(2, 2);
        let b = limiter.try_acquire(2, 2);
        assert!(a.is_some() && b.is_some());
        assert!(limiter.try_acquire(2, 2).is_none());
    }
}
