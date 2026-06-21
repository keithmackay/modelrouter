use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone)]
pub struct SessionPin {
    pub provider: String,
    pub model: String,
    last_seen: u64,
}

pub struct SessionAffinityMap {
    ttl_secs: u64,
    map: DashMap<String, SessionPin>,
    count: AtomicU64,
}

impl SessionAffinityMap {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            ttl_secs,
            map: DashMap::new(),
            count: AtomicU64::new(0),
        }
    }

    /// Look up a non-expired pin. Refreshes TTL on hit.
    pub fn get(&self, session_id: &str) -> Option<SessionPin> {
        let mut entry = self.map.get_mut(session_id)?;
        let now = now_secs();
        if now.saturating_sub(entry.last_seen) > self.ttl_secs {
            drop(entry);
            self.map.remove(session_id);
            self.count.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        entry.last_seen = now;
        Some(entry.clone())
    }

    /// Store or overwrite a pin.
    pub fn set(&self, session_id: &str, provider: &str, model: &str) {
        let is_new = !self.map.contains_key(session_id);
        self.map.insert(session_id.to_string(), SessionPin {
            provider: provider.to_string(),
            model: model.to_string(),
            last_seen: now_secs(),
        });
        if is_new {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Approximate number of live sessions (may include not-yet-swept expired entries).
    pub fn len(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Evict all expired entries. Call periodically from a background task.
    pub fn evict_expired(&self) {
        let now = now_secs();
        let ttl = self.ttl_secs;
        let mut removed = 0u64;
        self.map.retain(|_, pin| {
            let keep = now.saturating_sub(pin.last_seen) <= ttl;
            if !keep {
                removed += 1;
            }
            keep
        });
        if removed > 0 {
            self.count.fetch_sub(removed, Ordering::Relaxed);
        }
    }
}

/// Given an existing pin and the newly-resolved (provider, model), return
/// the (provider, model) to actually use and whether the pin should be updated.
pub fn resolve_with_pin(
    pin: Option<&SessionPin>,
    resolved_provider: &str,
    resolved_model: &str,
) -> (String, String, bool) {
    match pin {
        None => {
            // No pin yet — use resolved, store new pin
            (resolved_provider.to_string(), resolved_model.to_string(), true)
        }
        Some(p) if p.provider == resolved_provider && p.model == resolved_model => {
            // Exact match — use pin, refresh TTL
            (p.provider.clone(), p.model.clone(), true)
        }
        Some(p) if p.provider == resolved_provider => {
            // Same provider, different model — keep provider sticky, switch model, update pin
            tracing::debug!(
                provider = p.provider.as_str(),
                old_model = p.model.as_str(),
                new_model = resolved_model,
                "session model change within same provider — re-pinning model"
            );
            (p.provider.clone(), resolved_model.to_string(), true)
        }
        Some(p) => {
            // Different provider — caller switched providers; clear pin, use resolved
            tracing::debug!(
                old_provider = p.provider.as_str(),
                new_provider = resolved_provider,
                "session provider change — clearing pin"
            );
            (resolved_provider.to_string(), resolved_model.to_string(), true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pin_returns_resolved_and_requests_update() {
        let (p, m, update) = resolve_with_pin(None, "anthropic", "claude-opus-4-5");
        assert_eq!(p, "anthropic");
        assert_eq!(m, "claude-opus-4-5");
        assert!(update);
    }

    #[test]
    fn exact_match_uses_pin() {
        let pin = SessionPin { provider: "anthropic".into(), model: "claude-opus-4-5".into(), last_seen: 0 };
        let (p, m, update) = resolve_with_pin(Some(&pin), "anthropic", "claude-opus-4-5");
        assert_eq!(p, "anthropic");
        assert_eq!(m, "claude-opus-4-5");
        assert!(update);
    }

    #[test]
    fn same_provider_different_model_keeps_provider() {
        let pin = SessionPin { provider: "anthropic".into(), model: "claude-haiku-4-5".into(), last_seen: 0 };
        let (p, m, update) = resolve_with_pin(Some(&pin), "anthropic", "claude-opus-4-5");
        assert_eq!(p, "anthropic");
        assert_eq!(m, "claude-opus-4-5");
        assert!(update);
    }

    #[test]
    fn different_provider_clears_pin() {
        let pin = SessionPin { provider: "anthropic".into(), model: "claude-opus-4-5".into(), last_seen: 0 };
        let (p, m, update) = resolve_with_pin(Some(&pin), "openai", "gpt-4o");
        assert_eq!(p, "openai");
        assert_eq!(m, "gpt-4o");
        assert!(update);
    }

    #[test]
    fn map_set_and_get() {
        let map = SessionAffinityMap::new(1800);
        map.set("sess1", "anthropic", "claude-opus-4-5");
        let pin = map.get("sess1").unwrap();
        assert_eq!(pin.provider, "anthropic");
        assert_eq!(pin.model, "claude-opus-4-5");
    }

    #[test]
    fn map_expired_entry_returns_none() {
        let map = SessionAffinityMap::new(1800);
        map.set("sess1", "anthropic", "claude-opus-4-5");
        if let Some(mut e) = map.map.get_mut("sess1") {
            e.last_seen = 0; // force expiry
        }
        assert!(map.get("sess1").is_none());
    }

    #[test]
    fn evict_expired_removes_old_entries() {
        let map = SessionAffinityMap::new(1800);
        map.set("old", "openai", "gpt-4o");
        map.set("new", "anthropic", "claude-haiku-4-5");
        if let Some(mut e) = map.map.get_mut("old") {
            e.last_seen = 0; // force expiry
        }
        map.evict_expired();
        assert!(map.get("old").is_none());
        assert!(map.get("new").is_some());
    }
}
