use dashmap::DashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

struct SessionBucket {
    window_key: String,
    request_count: u32,
    token_count: u32,
}

pub struct SessionLimiter {
    tpm: u32,
    rpm: u32,
    buckets: DashMap<String, Mutex<SessionBucket>>,
}

impl SessionLimiter {
    pub fn new(tpm: u32, rpm: u32) -> Self {
        Self { tpm, rpm, buckets: DashMap::new() }
    }

    fn current_minute_key() -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        (secs / 60).to_string()
    }

    /// Returns true if request is allowed; records tokens if so.
    pub fn check_and_record(&self, session_id: &str, tokens: u32) -> bool {
        if self.tpm == 0 && self.rpm == 0 {
            return true;
        }
        let window = Self::current_minute_key();
        let entry = self.buckets
            .entry(session_id.to_string())
            .or_insert_with(|| Mutex::new(SessionBucket {
                window_key: window.clone(),
                request_count: 0,
                token_count: 0,
            }));
        let mut bucket = entry.lock().unwrap();
        if bucket.window_key != window {
            bucket.window_key = window;
            bucket.request_count = 0;
            bucket.token_count = 0;
        }
        if self.rpm > 0 && bucket.request_count >= self.rpm {
            return false;
        }
        if self.tpm > 0 && bucket.token_count + tokens > self.tpm {
            return false;
        }
        bucket.request_count += 1;
        bucket.token_count += tokens;
        true
    }
}
