use modelrouter::router::cache::{make_cache_key, ResponseCache};
use modelrouter::config::schema::CacheConfig;
use modelrouter::providers::adapter::CompletionResult;
use serde_json::json;

fn enabled_cache(max_entries: u64, ttl_seconds: u64) -> ResponseCache {
    ResponseCache::new(&CacheConfig {
        enabled: true,
        max_entries,
        ttl_seconds,
    })
}

#[tokio::test]
async fn cache_miss_returns_none() {
    let cache = enabled_cache(100, 60);
    assert!(cache.get("nonexistent-key").await.is_none());
}

#[tokio::test]
async fn cache_hit_returns_value() {
    let cache = enabled_cache(100, 60);
    let result = CompletionResult {
        content: "cached!".to_string(),
        prompt_tokens: 5,
        completion_tokens: 3,
        finish_reason: "stop".to_string(),
    };
    cache.insert("key-1".to_string(), result.clone()).await;
    let hit = cache.get("key-1").await.unwrap();
    assert_eq!(hit.content, "cached!");
    assert_eq!(hit.prompt_tokens, 5);
}

#[tokio::test]
async fn disabled_cache_always_misses() {
    let cache = ResponseCache::new(&CacheConfig {
        enabled: false,
        max_entries: 100,
        ttl_seconds: 60,
    });
    let result = CompletionResult {
        content: "ignored".to_string(),
        prompt_tokens: 1,
        completion_tokens: 1,
        finish_reason: "stop".to_string(),
    };
    cache.insert("key".to_string(), result).await;
    assert!(cache.get("key").await.is_none());
}

#[test]
fn same_inputs_produce_same_key() {
    let messages = vec![json!({"role": "user", "content": "hello"})];
    let k1 = make_cache_key("gpt-4o", &messages, Some(0.7), Some(100));
    let k2 = make_cache_key("gpt-4o", &messages, Some(0.7), Some(100));
    assert_eq!(k1, k2);
}

#[test]
fn different_model_produces_different_key() {
    let messages = vec![json!({"role": "user", "content": "hello"})];
    let k1 = make_cache_key("gpt-4o", &messages, None, None);
    let k2 = make_cache_key("gpt-4o-mini", &messages, None, None);
    assert_ne!(k1, k2);
}

#[test]
fn different_messages_produce_different_key() {
    let m1 = vec![json!({"role": "user", "content": "hello"})];
    let m2 = vec![json!({"role": "user", "content": "world"})];
    let k1 = make_cache_key("gpt-4o", &m1, None, None);
    let k2 = make_cache_key("gpt-4o", &m2, None, None);
    assert_ne!(k1, k2);
}
