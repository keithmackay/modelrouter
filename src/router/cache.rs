use serde_json::Value;
use crate::config::schema::CacheConfig;
use crate::providers::adapter::CompletionResult;

pub struct ResponseCache {
    inner: Option<moka::future::Cache<String, CompletionResult>>,
}

impl ResponseCache {
    pub fn new(config: &CacheConfig) -> Self {
        if !config.enabled {
            return Self { inner: None };
        }
        let cache = moka::future::Cache::builder()
            .max_capacity(config.max_entries)
            .time_to_live(std::time::Duration::from_secs(config.ttl_seconds))
            .build();
        Self { inner: Some(cache) }
    }

    pub async fn get(&self, key: &str) -> Option<CompletionResult> {
        self.inner.as_ref()?.get(key).await
    }

    pub async fn insert(&self, key: String, value: CompletionResult) {
        if let Some(ref cache) = self.inner {
            cache.insert(key, value).await;
        }
    }
}

/// Build a deterministic cache key from the full request body.
/// Returns a hex-encoded SHA-256 of the body with the `stream` field removed
/// (stream is not part of the cache identity).
pub fn make_cache_key(body: &Value) -> String {
    use sha2::{Digest, Sha256};
    // Clone and remove stream flag before hashing (stream is not part of the cache identity)
    let mut canonical = body.clone();
    if let Some(obj) = canonical.as_object_mut() {
        obj.remove("stream");
    }
    let mut hasher = Sha256::new();
    hasher.update(
        serde_json::to_string(&canonical)
            .unwrap_or_default()
            .as_bytes(),
    );
    hex::encode(hasher.finalize())
}
