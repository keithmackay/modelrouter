//! Post-response work that must finish before the process exits.
//!
//! Chat completions and `/v1/messages` wait for the cost-ledger row before
//! the caller sees the finished response. The other routes still write
//! ledger rows, prompt rows and cache stores from tasks spawned once the
//! response is on its way, so a slow store does not delay those callers.
//! The drain waits for in-flight *requests*; without this tracker, `serve`
//! would return as soon as the last response body was written and the
//! runtime would drop any task still writing its row.
//!
//! [`spawn`] counts the task until it completes; `serve` calls
//! [`wait_idle`] after the drain. One process-wide tracker: the work belongs
//! to the process, not to a router instance.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::sync::Notify;

#[derive(Default)]
pub struct Tracker {
    pending: AtomicUsize,
    idle: Notify,
}

struct Guard(Arc<Tracker>);

impl Drop for Guard {
    fn drop(&mut self) {
        if self.0.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
            self.0.idle.notify_one();
        }
    }
}

impl Tracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Spawn `fut` on the runtime and count it until it finishes (or panics).
    pub fn spawn<F>(self: &Arc<Self>, fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.pending.fetch_add(1, Ordering::AcqRel);
        let guard = Guard(self.clone());
        tokio::spawn(async move {
            let _guard = guard;
            fut.await
        })
    }

    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    /// Resolve once nothing is pending, or after `limit`. Returns how many
    /// tasks were still pending when it gave up (0 when it did not).
    pub async fn wait_idle(&self, limit: Duration) -> usize {
        let wait = async {
            loop {
                let notified = self.idle.notified();
                if self.pending() == 0 {
                    return;
                }
                notified.await;
            }
        };
        match tokio::time::timeout(limit, wait).await {
            Ok(()) => 0,
            Err(_) => self.pending(),
        }
    }
}

static TRACKER: LazyLock<Arc<Tracker>> = LazyLock::new(Tracker::new);

/// Spawn post-response work that shutdown must wait for.
pub fn spawn<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    TRACKER.spawn(fut)
}

/// Tracked tasks not yet finished.
pub fn pending() -> usize {
    TRACKER.pending()
}

/// Wait (at most `limit`) for every tracked task; returns how many were left.
pub async fn wait_idle(limit: Duration) -> usize {
    TRACKER.wait_idle(limit).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_idle_waits_for_the_last_task() {
        let t = Tracker::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = done.clone();
        t.spawn(async move {
            let _ = rx.await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            d.store(true, Ordering::Release);
        });
        let quick = t.spawn(async {});
        quick.await.unwrap();
        assert_eq!(t.pending(), 1);

        let waiter = {
            let t = t.clone();
            tokio::spawn(async move { t.wait_idle(Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!waiter.is_finished(), "returned while a task was pending");
        tx.send(()).unwrap();
        assert_eq!(waiter.await.unwrap(), 0);
        assert!(done.load(Ordering::Acquire), "the task finished before wait_idle returned");
        assert_eq!(t.pending(), 0);
    }

    #[tokio::test]
    async fn wait_idle_gives_up_after_the_limit_and_reports_what_is_left() {
        let t = Tracker::new();
        t.spawn(std::future::pending::<()>());
        t.spawn(std::future::pending::<()>());
        assert_eq!(t.wait_idle(Duration::from_millis(20)).await, 2);
    }

    #[tokio::test]
    async fn idle_tracker_returns_at_once() {
        let t = Tracker::new();
        assert_eq!(t.wait_idle(Duration::from_secs(5)).await, 0);
    }

    #[tokio::test]
    async fn a_panicking_task_is_still_released() {
        let t = Tracker::new();
        let h = t.spawn(async { panic!("boom") });
        assert!(h.await.is_err());
        assert_eq!(t.pending(), 0);
    }

    #[tokio::test]
    async fn the_process_tracker_counts_spawned_work() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let before = pending();
        let h = spawn(async move {
            let _ = rx.await;
        });
        assert!(pending() >= before + 1);
        tx.send(()).unwrap();
        h.await.unwrap();
        // Other tests share the process tracker, so only check it returns.
        wait_idle(Duration::from_millis(50)).await;
    }
}
