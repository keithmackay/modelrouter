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

/// Longest a request waits on Redis for one command or one dial. A Redis
/// that answers slowly must degrade to the local fallback like one that is
/// down, never stall every request behind it.
const COMMAND_TIMEOUT: Duration = Duration::from_millis(250);

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
        let dialed = tokio::time::timeout(
            COMMAND_TIMEOUT,
            self.client.get_multiplexed_async_connection(),
        )
        .await
        .unwrap_or_else(|_| Err(timed_out("connect")));
        match dialed {
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
        let outcome = tokio::time::timeout(COMMAND_TIMEOUT, cmd.query_async::<T>(&mut conn))
            .await
            .unwrap_or_else(|_| Err(timed_out(op)));
        match outcome {
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

fn timed_out(op: &str) -> redis::RedisError {
    redis::RedisError::from((
        redis::ErrorKind::Io,
        "cluster redis timed out",
        format!("{op} took longer than {COMMAND_TIMEOUT:?}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A server that accepts and never answers: the slow-Redis case.
    async fn black_hole() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                if let Ok((sock, _)) = listener.accept().await {
                    held.push(sock);
                }
            }
        });
        format!("redis://{addr}")
    }

    #[tokio::test]
    async fn a_silent_server_costs_one_timeout_then_cheap_misses() {
        let link = RedisLink::open(&black_hole().await, "t").unwrap();
        let started = Instant::now();
        let first: Option<String> = link.run("ping", &redis::cmd("PING")).await;
        assert!(first.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "bounded by the timeout: {:?}",
            started.elapsed()
        );

        let again = Instant::now();
        let second: Option<String> = link.run("ping", &redis::cmd("PING")).await;
        assert!(second.is_none());
        assert!(
            again.elapsed() < Duration::from_millis(50),
            "inside the backoff window nothing is dialled"
        );
    }

    #[tokio::test]
    async fn a_closed_port_is_a_fast_none() {
        let link = RedisLink::open("redis://127.0.0.1:1", "t").unwrap();
        let started = Instant::now();
        let r: Option<String> = link.run("ping", &redis::cmd("PING")).await;
        assert!(r.is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
