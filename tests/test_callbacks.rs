mod common;
use modelrouter::callbacks::{CallbackDispatcher, CallbackEvent};

#[tokio::test]
async fn dispatcher_with_no_backends_is_a_no_op() {
    let dispatcher = CallbackDispatcher::new(vec![]);
    dispatcher.dispatch(CallbackEvent {
        trace_id: "test-id".to_string(),
        user_id: 1,
        model: "gpt-4o".to_string(),
        provider: "openai".to_string(),
        input: serde_json::json!([{"role": "user", "content": "Hello"}]),
        output: "Hello back".to_string(),
        prompt_tokens: 10,
        completion_tokens: 5,
        cost_usd: 0.001,
        latency_ms: 200,
    });
    // No panic = pass
}

// ── Webhook deliveries ────────────────────────────────────────────────────────

use axum::{body::Bytes, extract::State, http::HeaderMap, http::StatusCode, routing::post, Router};
use hmac::{Hmac, Mac};
use modelrouter::callbacks::webhook::{WebhookBackend, WebhookBackendConfig};
use modelrouter::callbacks::CallbackBackend;
use modelrouter::db::models::CostLedgerEntry;
use serde_json::Value;
use std::sync::{Arc, Mutex};

const SECRET: &str = "test-signing-secret";

/// One POST as the receiver saw it.
#[derive(Clone, Debug)]
struct Delivery {
    headers: HeaderMap,
    body: Bytes,
}

impl Delivery {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

/// A local webhook endpoint on an ephemeral port that records every POST and
/// answers 204.
#[derive(Clone, Default)]
struct Receiver(Arc<Mutex<Vec<Delivery>>>);

async fn record(State(r): State<Receiver>, headers: HeaderMap, body: Bytes) -> StatusCode {
    r.0.lock().unwrap().push(Delivery { headers, body });
    StatusCode::NO_CONTENT
}

impl Receiver {
    async fn start() -> (Self, String) {
        let r = Receiver::default();
        let app = Router::new().route("/hook", post(record)).with_state(r.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (r, format!("http://{addr}/hook"))
    }

    async fn wait_for(&self, want: usize) -> Vec<Delivery> {
        for _ in 0..200 {
            let got = self.0.lock().unwrap().clone();
            if got.len() >= want {
                return got;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {want} webhook deliveries");
    }

    /// Wait for `want` deliveries, then a little longer, so a delivery that
    /// should not happen has had the chance to.
    async fn settle(&self, want: usize) -> Vec<Delivery> {
        self.wait_for(want).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        self.0.lock().unwrap().clone()
    }
}

fn backend(url: &str, events: &[&str], signing_secret: Option<&str>) -> WebhookBackend {
    WebhookBackend::new(WebhookBackendConfig {
        name: "test".into(),
        url: url.into(),
        events: events.iter().map(|e| e.to_string()).collect(),
        secret_header_name: Some("X-Static-Token".into()),
        secret_header_value: Some("static".into()),
        signing_secret: signing_secret.map(str::to_string),
    })
}

fn ledger_row() -> CostLedgerEntry {
    CostLedgerEntry {
        id: 9,
        user_id: 1,
        prompt_id: 0,
        model: "m".into(),
        provider: "p".into(),
        project: Some("alpha".into()),
        tokens_in: 10,
        tokens_out: 5,
        cost_usd: 0.25,
        created_at: "2026-11-14T15:02:11+00:00".into(),
        api_key_id: None,
        cache_hit: false,
        saved_usd: 0.0,
        attribution_correlation_id: Some("task-17".into()),
        attribution_tags: r#"{"agent":"worker"}"#.into(),
        experiment_id: None,
        experiment_variant: None,
        tokens_estimated: false,
        call_kind: "chat".into(),
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    }
}

fn completion() -> CallbackEvent {
    CallbackEvent {
        trace_id: "42".into(),
        user_id: 1,
        model: "m".into(),
        provider: "p".into(),
        input: serde_json::json!([]),
        output: "out".into(),
        prompt_tokens: 10,
        completion_tokens: 5,
        cost_usd: 0.25,
        latency_ms: 7,
    }
}

/// What a receiver computes: HMAC-SHA256 over `<timestamp>.<raw body>`.
fn expected_signature(timestamp: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

#[tokio::test]
async fn a_signed_usage_delivery_verifies_against_the_bytes_received() {
    let (receiver, url) = Receiver::start().await;
    let dispatcher = CallbackDispatcher::new(vec![Box::new(backend(&url, &["usage"], Some(SECRET)))]);
    dispatcher.dispatch_usage(&ledger_row());

    let got = receiver.wait_for(1).await;
    let d = &got[0];
    assert_eq!(d.header("x-modelrouter-event"), Some("usage"));
    assert_eq!(d.header("content-type"), Some("application/json"));
    let ts = d.header("x-modelrouter-timestamp").expect("timestamp header");
    let age = chrono::Utc::now().timestamp() - ts.parse::<i64>().unwrap();
    assert!((0..5).contains(&age), "timestamp {ts} is {age}s old");
    assert_eq!(
        d.header("x-modelrouter-signature"),
        Some(expected_signature(ts, &d.body).as_str())
    );

    let delivery = d.header("x-modelrouter-delivery").expect("delivery header");
    assert!(uuid::Uuid::parse_str(delivery).is_ok(), "{delivery}");
    let mut expected = modelrouter::callbacks::usage::from_ledger(&ledger_row());
    expected["delivery_id"] = Value::String(delivery.to_string());
    assert_eq!(d.json(), expected);
    // The static secret header still travels alongside the signature.
    assert_eq!(d.header("x-static-token"), Some("static"));
}

#[tokio::test]
async fn an_unsigned_webhook_sends_no_signature_headers() {
    let (receiver, url) = Receiver::start().await;
    let b = backend(&url, &["usage", "completion"], None);
    b.send_usage(&modelrouter::callbacks::usage::from_ledger(&ledger_row()));
    b.send(completion());

    for d in receiver.wait_for(2).await {
        for h in ["x-modelrouter-signature", "x-modelrouter-timestamp", "x-modelrouter-delivery"] {
            assert!(d.header(h).is_none(), "{h} on an unsigned delivery");
        }
        assert_eq!(d.header("x-static-token"), Some("static"));
    }
}

#[tokio::test]
async fn a_signed_completion_keeps_its_payload_and_is_signed() {
    let (receiver, url) = Receiver::start().await;
    backend(&url, &["completion"], Some(SECRET)).send(completion());

    let got = receiver.wait_for(1).await;
    let d = &got[0];
    assert_eq!(d.header("x-modelrouter-event"), Some("completion"));
    let ts = d.header("x-modelrouter-timestamp").unwrap();
    assert_eq!(
        d.header("x-modelrouter-signature"),
        Some(expected_signature(ts, &d.body).as_str())
    );
    assert_eq!(
        d.json(),
        serde_json::json!({
            "event": "completion",
            "trace_id": "42",
            "user_id": 1,
            "model": "m",
            "provider": "p",
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "cost_usd": 0.25,
            "latency_ms": 7,
        })
    );
}

#[tokio::test]
async fn a_completion_subscription_receives_no_usage_event() {
    let (receiver, url) = Receiver::start().await;
    let b = backend(&url, &["completion"], None);
    b.send_usage(&modelrouter::callbacks::usage::from_ledger(&ledger_row()));
    b.send(completion());

    let got = receiver.settle(1).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].json()["event"], "completion");
}

#[tokio::test]
async fn a_usage_subscription_receives_no_completion_event() {
    let (receiver, url) = Receiver::start().await;
    let b = backend(&url, &["usage"], None);
    b.send(completion());
    b.send_usage(&modelrouter::callbacks::usage::from_ledger(&ledger_row()));

    let got = receiver.settle(1).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].json()["event"], "usage");
}
