// src/api/middleware/ip_rate_limit.rs
use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use dashmap::DashMap;
use std::net::SocketAddr;
use std::sync::Arc;

pub struct IpRateLimiter {
    counts: DashMap<(String, String), u64>,
    limit_rpm: u32,
}

impl IpRateLimiter {
    pub fn new(limit_rpm: u32) -> Self {
        Self { counts: DashMap::new(), limit_rpm }
    }

    /// Returns true if request is allowed. Increments counter.
    pub fn check_and_increment(&self, ip: &str) -> bool {
        if self.limit_rpm == 0 {
            return true;
        }
        let bucket = chrono::Utc::now().format("%Y-%m-%dT%H:%M").to_string();
        let key = (ip.to_string(), bucket);
        let mut count = self.counts.entry(key).or_insert(0);
        *count += 1;
        *count <= self.limit_rpm as u64
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
        if !limiter.check_and_increment(&ip) {
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(Body::from("rate limit exceeded"))
                .unwrap();
        }
    }
    next.run(request).await
}
