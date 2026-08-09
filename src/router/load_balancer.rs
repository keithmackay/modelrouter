use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

pub use crate::config::schema::{LbStrategy, LbPoolEntry, LoadBalancerConfig};

struct Pool {
    /// Expanded entry indices for weighted round-robin.
    /// For weights [2, 1], expanded = [0, 0, 1].
    expanded: Vec<usize>,
    entries: Vec<(String, String)>, // (provider, model)
    counter: AtomicUsize,
}

impl Pool {
    fn new(config: &LoadBalancerConfig) -> Self {
        let entries: Vec<(String, String)> = config
            .pool
            .iter()
            .map(|e| (e.provider.clone(), e.model.clone()))
            .collect();

        let expanded = match config.strategy {
            LbStrategy::RoundRobin => (0..entries.len()).collect(),
            LbStrategy::Weighted => {
                let mut exp = Vec::new();
                for (i, entry) in config.pool.iter().enumerate() {
                    for _ in 0..entry.weight {
                        exp.push(i);
                    }
                }
                exp
            }
        };

        Self {
            expanded,
            entries,
            counter: AtomicUsize::new(0),
        }
    }

    /// Next entry satisfying `accept`. Scans at most one full cycle of the expanded
    /// ring so a pool whose entries are all disabled terminates instead of spinning.
    fn next_where(&self, accept: &dyn Fn(&str, &str) -> bool) -> Option<(String, String)> {
        if self.expanded.is_empty() {
            return None;
        }
        for _ in 0..self.expanded.len() {
            let candidate = self.next()?;
            if accept(&candidate.0, &candidate.1) {
                return Some(candidate);
            }
        }
        None
    }

    fn next(&self) -> Option<(String, String)> {
        if self.expanded.is_empty() {
            return None;
        }
        // Relaxed ordering is sufficient — no cross-thread memory sync required.
        // usize wraparound is safe (wraps to 0) but may break a cycle at the boundary;
        // this is negligible at normal request rates (takes ~centuries on 64-bit).
        let idx = self.counter.fetch_add(1, Ordering::Relaxed) % self.expanded.len();
        let entry_idx = self.expanded[idx];
        self.entries.get(entry_idx).cloned()
    }
}

pub struct LoadBalancer {
    pools: HashMap<String, Pool>,
}

impl LoadBalancer {
    /// Construct from a map of pool names to configurations.
    pub fn new(configs: HashMap<String, LoadBalancerConfig>) -> Self {
        let pools = configs
            .into_iter()
            .map(|(name, config)| (name, Pool::new(&config)))
            .collect();
        Self { pools }
    }

    /// If `model` is a named load balancer pool, returns the next `(provider, model)` to use.
    /// Returns `None` if `model` is not a load balancer pool name — caller uses normal routing.
    pub fn resolve(&self, model: &str) -> Option<(String, String)> {
        self.pools.get(model)?.next()
    }

    /// Like [`LoadBalancer::resolve`], but skips entries `accept` rejects — used to
    /// keep operator-disabled models and providers out of the pool (issue #5).
    ///
    /// Returns `None` both when `model` is not a pool and when every entry in the
    /// pool is rejected; callers distinguish the two with [`LoadBalancer::is_pool`].
    pub fn resolve_available(
        &self,
        model: &str,
        accept: impl Fn(&str, &str) -> bool,
    ) -> Option<(String, String)> {
        self.pools.get(model)?.next_where(&accept)
    }

    /// Whether `model` names a configured pool.
    pub fn is_pool(&self, model: &str) -> bool {
        self.pools.contains_key(model)
    }
}
