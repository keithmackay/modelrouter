use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use serde_json::Value;

use super::{CallbackBackend, CallbackEvent};
use crate::db::repositories::webhook_callbacks::WebhookCallback;

pub struct WebhookBackendConfig {
    pub name: String,
    pub url: String,
    pub events: Vec<String>,
    pub secret_header_name: Option<String>,
    pub secret_header_value: Option<String>,
    /// When set, every delivery is signed (see [`signature`]) and carries the
    /// `X-Modelrouter-*` headers.
    pub signing_secret: Option<String>,
}

impl WebhookBackendConfig {
    /// The backend for a stored webhook row. Unparseable `events` subscribe to
    /// nothing in particular, which [`WebhookBackend`] treats as every event.
    pub fn from_row(row: WebhookCallback) -> Self {
        Self {
            events: serde_json::from_str(&row.events).unwrap_or_default(),
            name: row.name,
            url: row.url,
            secret_header_name: row.secret_header_name,
            secret_header_value: row.secret_header_value,
            signing_secret: row.signing_secret,
        }
    }
}

/// `sha256=` + hex HMAC-SHA256, keyed by `secret`, over
/// `<unix timestamp>.<raw body>`. A receiver recomputes it from the
/// `X-Modelrouter-Timestamp` header and the exact bytes it received.
pub fn signature(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// How a webhook's deliveries are attempted.
#[derive(Clone, Debug)]
pub struct DeliveryPolicy {
    /// Limit on one attempt, from connecting to the full response.
    pub timeout: Duration,
    /// Wait before each retry of a `usage` delivery; one retry per entry.
    pub retry_delays: Vec<Duration>,
    /// Deliveries in flight for one webhook beyond which a new one is dropped.
    pub max_pending: usize,
}

impl Default for DeliveryPolicy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            retry_delays: [1, 5, 30, 120, 600].map(Duration::from_secs).to_vec(),
            max_pending: 1000,
        }
    }
}

pub struct WebhookBackend {
    config: Arc<WebhookBackendConfig>,
    policy: Arc<DeliveryPolicy>,
    client: reqwest::Client,
    pending: Arc<AtomicUsize>,
}

/// Counts one delivery as pending for as long as it lives.
struct PendingDelivery(Arc<AtomicUsize>);

impl Drop for PendingDelivery {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl WebhookBackend {
    pub fn new(config: WebhookBackendConfig) -> Self {
        Self::with_policy(config, DeliveryPolicy::default())
    }

    pub fn with_policy(config: WebhookBackendConfig, policy: DeliveryPolicy) -> Self {
        // Redirects are not followed: a signed body must reach the URL the
        // operator registered, and nowhere else.
        let client = reqwest::Client::builder()
            .timeout(policy.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("a client with a timeout and no redirects always builds");
        Self {
            config: Arc::new(config),
            policy: Arc::new(policy),
            client,
            pending: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// True when this webhook subscribes to `event`; an empty list or `*`
    /// subscribes to every event.
    fn should_send_event(&self, event: &str) -> bool {
        self.config.events.is_empty()
            || self.config.events.iter().any(|e| e == "*" || e == event)
    }

    /// Claim a pending slot, or `None` when the webhook already has
    /// `max_pending` deliveries in flight.
    fn claim_pending(&self) -> Option<PendingDelivery> {
        let claimed = self
            .pending
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < self.policy.max_pending).then_some(n + 1)
            })
            .is_ok();
        claimed.then(|| PendingDelivery(self.pending.clone()))
    }

    /// POST `body` in the background. `delivery_id` names the delivery in the
    /// `X-Modelrouter-Delivery` header of a signed webhook and stays the same
    /// across retries. With `retry`, a connection error, timeout, 429 or 5xx is
    /// retried on the policy's schedule; any other answer ends the delivery.
    fn deliver(&self, event: &'static str, delivery_id: String, body: Value, retry: bool) {
        let Some(pending) = self.claim_pending() else {
            tracing::warn!(
                webhook = self.config.name.as_str(),
                event,
                pending = self.policy.max_pending,
                "webhook delivery dropped: too many deliveries pending"
            );
            return;
        };
        let config = self.config.clone();
        let policy = self.policy.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            let _pending = pending;
            let body = match serde_json::to_vec(&body) {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(webhook = config.name.as_str(), "webhook body not serializable: {e}");
                    return;
                }
            };
            let delays: &[Duration] = if retry { &policy.retry_delays } else { &[] };
            let mut attempt = 0;
            loop {
                attempt += 1;
                let failure = match post(&client, &config, event, &delivery_id, body.clone()).await {
                    Ok(resp) if resp.status().is_success() => return,
                    Ok(resp) if is_retryable(resp.status()) => format!("status {}", resp.status()),
                    Ok(resp) => {
                        tracing::warn!(
                            webhook = config.name.as_str(),
                            event,
                            delivery_id = delivery_id.as_str(),
                            status = resp.status().as_u16(),
                            "webhook delivery refused; not retrying"
                        );
                        return;
                    }
                    Err(e) => e.to_string(),
                };
                let Some(delay) = delays.get(attempt - 1) else {
                    tracing::warn!(
                        webhook = config.name.as_str(),
                        event,
                        delivery_id = delivery_id.as_str(),
                        attempts = attempt,
                        "webhook POST failed: {failure}"
                    );
                    return;
                };
                tracing::debug!(
                    webhook = config.name.as_str(),
                    delivery_id = delivery_id.as_str(),
                    attempt,
                    "webhook POST failed ({failure}); retrying in {delay:?}"
                );
                tokio::time::sleep(*delay).await;
            }
        });
    }
}

/// An answer worth retrying: the receiver is overloaded or failed.
fn is_retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// One POST of `body`, signed afresh when the webhook has a signing secret.
async fn post(
    client: &reqwest::Client,
    config: &WebhookBackendConfig,
    event: &str,
    delivery_id: &str,
    body: Vec<u8>,
) -> reqwest::Result<reqwest::Response> {
    let mut req = client
        .post(&config.url)
        .header(reqwest::header::CONTENT_TYPE, "application/json");
    if let (Some(h), Some(v)) = (&config.secret_header_name, &config.secret_header_value) {
        req = req.header(h, v);
    }
    if let Some(secret) = &config.signing_secret {
        let timestamp = chrono::Utc::now().timestamp();
        req = req
            .header("X-Modelrouter-Event", event)
            .header("X-Modelrouter-Delivery", delivery_id)
            .header("X-Modelrouter-Timestamp", timestamp.to_string())
            .header("X-Modelrouter-Signature", signature(secret, timestamp, &body));
    }
    req.body(body).send().await
}

impl CallbackBackend for WebhookBackend {
    fn send(&self, event: CallbackEvent) {
        if !self.should_send_event("completion") { return; }

        let body = serde_json::json!({
            "event": "completion",
            "trace_id": event.trace_id,
            "user_id": event.user_id,
            "model": event.model,
            "provider": event.provider,
            "prompt_tokens": event.prompt_tokens,
            "completion_tokens": event.completion_tokens,
            "cost_usd": event.cost_usd,
            "latency_ms": event.latency_ms,
        });
        self.deliver("completion", uuid::Uuid::new_v4().to_string(), body, false);
    }

    fn send_usage(&self, payload: &Value) {
        if !self.should_send_event(super::usage::EVENT) { return; }

        let delivery_id = uuid::Uuid::new_v4().to_string();
        let mut body = payload.clone();
        body["delivery_id"] = Value::String(delivery_id.clone());
        self.deliver(super::usage::EVENT, delivery_id, body, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_backend(events: Vec<&str>) -> WebhookBackend {
        WebhookBackend::new(WebhookBackendConfig {
            name: "test".into(),
            url: "http://example.com".into(),
            events: events.into_iter().map(str::to_string).collect(),
            secret_header_name: None,
            secret_header_value: None,
            signing_secret: None,
        })
    }

    #[test]
    fn wildcard_events_receive_every_event() {
        let b = make_backend(vec!["*"]);
        assert!(b.should_send_event("completion"));
        assert!(b.should_send_event("usage"));
    }

    #[test]
    fn empty_events_receive_every_event() {
        let b = make_backend(vec![]);
        assert!(b.should_send_event("completion"));
        assert!(b.should_send_event("usage"));
    }

    #[test]
    fn a_completion_subscription_receives_only_completions() {
        let b = make_backend(vec!["completion"]);
        assert!(b.should_send_event("completion"));
        assert!(!b.should_send_event("usage"));
    }

    #[test]
    fn a_usage_subscription_receives_only_usage() {
        let b = make_backend(vec!["usage"]);
        assert!(b.should_send_event("usage"));
        assert!(!b.should_send_event("completion"));
    }

    #[test]
    fn an_unrelated_subscription_receives_nothing() {
        let b = make_backend(vec!["budget_exceeded"]);
        assert!(!b.should_send_event("completion"));
        assert!(!b.should_send_event("usage"));
    }

    /// Expected value computed independently:
    /// `printf '%s' '1700000000.{"event":"usage","ledger_id":1}' |
    ///  openssl dgst -sha256 -hmac 'test-signing-secret'`
    #[test]
    fn the_signature_is_hmac_sha256_over_timestamp_dot_body() {
        assert_eq!(
            signature("test-signing-secret", 1_700_000_000, br#"{"event":"usage","ledger_id":1}"#),
            "sha256=8da88f0679dcb58f7c27365a94250be2f5ab49f2877f8d9dcda6cc53c54f17df"
        );
    }

    #[test]
    fn a_stored_row_becomes_a_backend_config() {
        let row = crate::db::repositories::webhook_callbacks::WebhookCallback {
            id: 1,
            name: "coordinator".into(),
            url: "http://127.0.0.1:9/hook".into(),
            events: r#"["usage"]"#.into(),
            secret_header_name: Some("X-Token".into()),
            secret_header_value: Some("t".into()),
            signing_secret: Some("s".into()),
            enabled: true,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        let cfg = WebhookBackendConfig::from_row(row);
        assert_eq!(cfg.name, "coordinator");
        assert_eq!(cfg.url, "http://127.0.0.1:9/hook");
        assert_eq!(cfg.events, vec!["usage".to_string()]);
        assert_eq!(cfg.secret_header_name.as_deref(), Some("X-Token"));
        assert_eq!(cfg.secret_header_value.as_deref(), Some("t"));
        assert_eq!(cfg.signing_secret.as_deref(), Some("s"));
    }
}
