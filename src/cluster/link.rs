//! One Redis connection for every cross-replica counter and pin.
//!
//! Holds a multiplexed connection and re-dials it after an error, gated by
//! the same reconnect backoff the response cache uses, so a dead Redis costs
//! each request a cheap `None` rather than a connect timeout. Callers decide
//! what `None` means: the limiters fall back to their per-replica share, the
//! OIDC store fails the login.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use redis::aio::MultiplexedConnection;
use tokio::sync::RwLock;

use crate::router::cache::store::{LinkTransition, ReconnectGate};

const RETRY_COOLDOWN: Duration = Duration::from_secs(5);

pub struct RedisLink {
    client: redis::Client,
    namespace: String,
    conn: RwLock<Option<MultiplexedConnection>>,
    gate: Mutex<ReconnectGate>,
}

impl RedisLink {
    pub fn open(url: &str, namespace: &str) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            client: redis::Client::open(url)?,
            namespace: namespace.to_string(),
            conn: RwLock::new(None),
            gate: Mutex::new(ReconnectGate::new(RETRY_COOLDOWN)),
        }))
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn key(&self, suffix: &str) -> String {
        format!("{}:{suffix}", self.namespace)
    }

    /// The held connection, dialling if needed. `None` while the backoff
    /// window is armed, while another task is dialling, or when the dial
    /// failed.
    pub async fn conn(&self) -> Option<MultiplexedConnection> {
        if let Some(c) = self.conn.read().await.as_ref() {
            return Some(c.clone());
        }
        let Ok(mut guard) = self.conn.try_write() else {
            return None;
        };
        if let Some(c) = guard.as_ref() {
            return Some(c.clone());
        }
        if !self
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .should_attempt(Instant::now())
        {
            return None;
        }
        match self.client.get_multiplexed_async_connection().await {
            Ok(c) => {
                let transition = self
                    .gate
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .on_success();
                if matches!(
                    transition,
                    LinkTransition::Connected | LinkTransition::Recovered
                ) {
                    tracing::info!(namespace = %self.namespace, "cluster redis connected");
                }
                *guard = Some(c.clone());
                Some(c)
            }
            Err(e) => {
                self.note_failure("connect", &e);
                None
            }
        }
    }

    /// Run one command, dropping the connection on error so the next call
    /// re-dials. `None` on any failure.
    pub async fn run<T: redis::FromRedisValue>(
        &self,
        op: &'static str,
        cmd: &redis::Cmd,
    ) -> Option<T> {
        let mut conn = self.conn().await?;
        match cmd.query_async::<T>(&mut conn).await {
            Ok(v) => Some(v),
            Err(e) => {
                *self.conn.write().await = None;
                self.note_failure(op, &e);
                None
            }
        }
    }

    fn note_failure(&self, op: &'static str, err: &redis::RedisError) {
        let transition = self
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_failure(Instant::now());
        if transition == LinkTransition::Lost {
            tracing::warn!(
                error = %err,
                op,
                namespace = %self.namespace,
                "cluster redis became unreachable; limits fall back to this replica's share \
                 and admin logins started here must finish here until it returns"
            );
        } else {
            tracing::debug!(error = %err, op, "cluster redis still unreachable");
        }
    }
}
