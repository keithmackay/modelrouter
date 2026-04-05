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
}

impl ConcurrencyLimiter {
    pub fn new() -> Self {
        Self { semaphores: DashMap::new() }
    }

    /// Try to acquire a slot for `user_id` with capacity `max`.
    ///
    /// Returns `Some(permit)` if a slot was available, `None` if at capacity
    /// or `max` is 0. Hold the returned permit for the duration of the upstream
    /// call — dropping it releases the slot.
    pub fn try_acquire(&self, user_id: i64, max: u32) -> Option<OwnedSemaphorePermit> {
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
    fn default() -> Self { Self::new() }
}
