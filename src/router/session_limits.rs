//! Per-session requests-per-minute and tokens-per-minute buckets.
//!
//! In memory, a fixed one-minute window per session holding this replica's
//! share of the limits. With a cluster Redis link, one hash per session and
//! minute is checked and reserved atomically, so two replicas cannot both
//! admit on the last tokens. Redis unreachable falls back to the share.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;

use crate::cluster::RedisLink;
use crate::config::schema::ClusterConfig;

struct SessionBucket {
    window_key: String,
    request_count: u32,
    token_count: u32,
}

pub struct SessionLimiter {
    tpm: u32,
    rpm: u32,
    cluster: ClusterConfig,
    redis: Option<Arc<RedisLink>>,
    buckets: DashMap<String, Mutex<SessionBucket>>,
}

const RESERVE_SCRIPT: &str = r#"
local req = tonumber(redis.call('HGET', KEYS[1], 'req') or '0')
local tok = tonumber(redis.call('HGET', KEYS[1], 'tok') or '0')
if tonumber(ARGV[1]) > 0 and req >= tonumber(ARGV[1]) then
  return 0
end
if tonumber(ARGV[2]) > 0 and tok + tonumber(ARGV[3]) > tonumber(ARGV[2]) then
  return 0
end
redis.call('HINCRBY', KEYS[1], 'req', 1)
redis.call('HINCRBY', KEYS[1], 'tok', ARGV[3])
redis.call('EXPIRE', KEYS[1], ARGV[4])
return 1
"#;

impl SessionLimiter {
    /// Limits as configured for the whole deployment.
    pub fn new(tpm: u32, rpm: u32) -> Self {
        Self {
            tpm,
            rpm,
            cluster: ClusterConfig::default(),
            redis: None,
            buckets: DashMap::new(),
        }
    }

    /// Enforce this replica's share in memory.
    pub fn for_cluster(mut self, cluster: ClusterConfig) -> Self {
        self.cluster = cluster;
        self
    }

    /// Count across replicas in Redis, full limits; the share stays as fallback.
    pub fn with_redis(mut self, link: Option<Arc<RedisLink>>) -> Self {
        self.redis = link;
        self
    }

    fn current_minute_key() -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        (secs / 60).to_string()
    }

    /// Returns true if request is allowed; records tokens if so.
    pub async fn check_and_record(&self, session_id: &str, tokens: u32) -> bool {
        if self.tpm == 0 && self.rpm == 0 {
            return true;
        }
        if let Some(link) = &self.redis {
            let key = link.key(&format!("sess:{session_id}:{}", Self::current_minute_key()));
            let admitted: Option<i64> = link
                .run(
                    "session reserve",
                    redis::cmd("EVAL")
                        .arg(RESERVE_SCRIPT)
                        .arg(1)
                        .arg(&key)
                        .arg(self.rpm)
                        .arg(self.tpm)
                        .arg(tokens)
                        .arg(120),
                )
                .await;
            if let Some(admitted) = admitted {
                return admitted == 1;
            }
        }
        self.check_and_record_local(session_id, tokens)
    }

    fn check_and_record_local(&self, session_id: &str, tokens: u32) -> bool {
        let tpm = self.cluster.share(self.tpm);
        let rpm = self.cluster.share(self.rpm);
        let window = Self::current_minute_key();
        let entry = self
            .buckets
            .entry(session_id.to_string())
            .or_insert_with(|| {
                Mutex::new(SessionBucket {
                    window_key: window.clone(),
                    request_count: 0,
                    token_count: 0,
                })
            });
        let mut bucket = entry.lock().unwrap_or_else(|e| e.into_inner());
        if bucket.window_key != window {
            bucket.window_key = window;
            bucket.request_count = 0;
            bucket.token_count = 0;
        }
        if rpm > 0 && bucket.request_count >= rpm {
            return false;
        }
        if tpm > 0 && bucket.token_count + tokens > tpm {
            return false;
        }
        bucket.request_count += 1;
        bucket.token_count += tokens;
        true
    }
}
