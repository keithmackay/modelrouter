//! Per-IP requests-per-minute limit, applied to every route.
//!
//! In memory, a counter per (ip, minute) holding this replica's share. With
//! a cluster Redis link, one counter per (ip, minute) shared by all replicas.
//! Redis unreachable falls back to the share.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use dashmap::DashMap;

use crate::cluster::RedisLink;
use crate::config::schema::ClusterConfig;

pub struct IpRateLimiter {
    counts: DashMap<(String, String), u64>,
    limit_rpm: u32,
    cluster: ClusterConfig,
    redis: Option<Arc<RedisLink>>,
}

const COUNT_SCRIPT: &str = r#"
local n = redis.call('INCR', KEYS[1])
if n == 1 then
  redis.call('EXPIRE', KEYS[1], ARGV[1])
end
return n
"#;

impl IpRateLimiter {
    /// The limit as configured for the whole deployment. 0 disables.
    pub fn new(limit_rpm: u32) -> Self {
        Self {
            counts: DashMap::new(),
            limit_rpm,
            cluster: ClusterConfig::default(),
            redis: None,
        }
    }

    pub fn for_cluster(mut self, cluster: ClusterConfig) -> Self {
        self.cluster = cluster;
        self
    }

    pub fn with_redis(mut self, link: Option<Arc<RedisLink>>) -> Self {
        self.redis = link;
        self
    }

    /// Returns true if request is allowed. Increments counter.
    pub async fn check_and_increment(&self, ip: &str) -> bool {
        if self.limit_rpm == 0 {
            return true;
        }
        let bucket = chrono::Utc::now().format("%Y-%m-%dT%H:%M").to_string();
        if let Some(link) = &self.redis {
            let key = link.key(&format!("ip:{ip}:{bucket}"));
            let n: Option<i64> = link
                .run(
                    "ip count",
                    redis::cmd("EVAL")
                        .arg(COUNT_SCRIPT)
                        .arg(1)
                        .arg(&key)
                        .arg(120),
                )
                .await;
            if let Some(n) = n {
                return n <= self.limit_rpm as i64;
            }
        }
        let share = self.cluster.share(self.limit_rpm) as u64;
        let key = (ip.to_string(), bucket);
        let mut count = self.counts.entry(key).or_insert(0);
        *count += 1;
        *count <= share
    }
}

pub async fn ip_rate_limit_middleware(
    State(limiter): State<Arc<IpRateLimiter>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if let Some(ConnectInfo(addr)) = connect_info {
        let ip = addr.ip().to_string();
        if !limiter.check_and_increment(&ip).await {
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(Body::from("rate limit exceeded"))
                .unwrap();
        }
    }
    next.run(request).await
}
