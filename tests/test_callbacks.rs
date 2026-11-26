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

/// How the receiver answers one POST.
#[derive(Clone, Copy)]
enum Reply {
    Status(u16),
    /// Record the request, then never answer.
    Hang,
}

/// A local webhook endpoint on an ephemeral port that records every POST and
/// answers with the scripted replies in order, then 204.
#[derive(Clone, Default)]
struct Receiver {
    seen: Arc<Mutex<Vec<Delivery>>>,
    script: Arc<Mutex<std::collections::VecDeque<Reply>>>,
}

async fn record(State(r): State<Receiver>, headers: HeaderMap, body: Bytes) -> StatusCode {
    r.seen.lock().unwrap().push(Delivery { headers, body });
    let reply = r.script.lock().unwrap().pop_front().unwrap_or(Reply::Status(204));
    match reply {
        Reply::Status(code) => StatusCode::from_u16(code).unwrap(),
        Reply::Hang => std::future::pending().await,
    }
}

impl Receiver {
    async fn start() -> (Self, String) {
        Self::scripted(&[]).await
    }

    async fn scripted(replies: &[Reply]) -> (Self, String) {
        let r = Receiver::default();
        r.script.lock().unwrap().extend(replies.iter().copied());
        let app = Router::new().route("/hook", post(record)).with_state(r.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (r, format!("http://{addr}/hook"))
    }

    async fn wait_for(&self, want: usize) -> Vec<Delivery> {
        for _ in 0..200 {
            let got = self.seen.lock().unwrap().clone();
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
        self.seen.lock().unwrap().clone()
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
        request_fingerprint: None,
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
    let db = usage_store(&url, &["usage"], Some(SECRET)).await;
    pump_usage(&db, &DeliveryPolicy::default(), 1).await;

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
    let db = usage_store(&url, &["usage", "completion"], None).await;
    pump_usage(&db, &DeliveryPolicy::default(), 1).await;
    backend(&url, &["usage", "completion"], None).send(completion());

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
    let db = usage_store(&url, &["completion"], None).await;
    let n = modelrouter::callbacks::outbox::enqueue_usage(&db, &ledger_row()).await.unwrap();
    assert_eq!(n, 0, "a completion subscription is not a usage delivery");
    backend(&url, &["completion"], None).send(completion());

    let got = receiver.settle(1).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].json()["event"], "completion");
}

#[tokio::test]
async fn a_usage_subscription_receives_no_completion_event() {
    let (receiver, url) = Receiver::start().await;
    backend(&url, &["usage"], None).send(completion());
    let db = usage_store(&url, &["usage"], None).await;
    pump_usage(&db, &DeliveryPolicy::default(), 1).await;

    let got = receiver.settle(1).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].json()["event"], "usage");
}

// ── Timeouts, retries and the pending bound ───────────────────────────────────

use modelrouter::callbacks::webhook::DeliveryPolicy;
use std::time::Duration;

fn quick_policy() -> DeliveryPolicy {
    DeliveryPolicy {
        timeout: Duration::from_millis(200),
        retry_delays: vec![Duration::from_millis(10); 5],
        max_pending: 1000,
    }
}

async fn usage_store(url: &str, events: &[&str], signing_secret: Option<&str>) -> modelrouter::db::sqlite::SqliteDb {
    use modelrouter::db::repositories::webhook_callbacks::{NewWebhookCallback, WebhookCallbackRepository};
    let db = modelrouter::db::sqlite::SqliteDb::connect(":memory:").await.unwrap();
    modelrouter::db::migrations::run_migrations(&db.pool).await.unwrap();
    let events = serde_json::to_string(&events.iter().map(|e| (*e).to_string()).collect::<Vec<_>>()).unwrap();
    WebhookCallbackRepository::create_webhook(
        &db,
        NewWebhookCallback {
            name: "test".into(),
            url: url.into(),
            events,
            secret_header_name: Some("X-Static-Token".into()),
            secret_header_value: Some("static".into()),
            signing_secret: signing_secret.map(str::to_string),
        },
    )
    .await
    .unwrap();
    db
}

fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Enqueue one usage delivery and drain until it has been attempted `rounds`
/// times, waiting out the policy's first retry delay between passes.
async fn pump_usage(db: &modelrouter::db::sqlite::SqliteDb, policy: &DeliveryPolicy, rounds: usize) {
    modelrouter::callbacks::outbox::enqueue_usage(db, &ledger_row()).await.unwrap();
    let client = http_client(policy.timeout);
    for _ in 0..rounds {
        modelrouter::callbacks::outbox::drain_with(db, "test-worker", &client, policy).await;
        let pause = policy.retry_delays.first().copied().unwrap_or(Duration::from_millis(10))
            + Duration::from_millis(40);
        tokio::time::sleep(pause).await;
    }
}

#[test]
fn the_default_policy_is_a_ten_second_timeout_and_five_backed_off_retries() {
    let p = DeliveryPolicy::default();
    assert_eq!(p.timeout, Duration::from_secs(10));
    assert_eq!(
        p.retry_delays,
        [1, 5, 30, 120, 600].map(Duration::from_secs).to_vec()
    );
    assert_eq!(p.max_pending, 1000);
}

#[tokio::test]
async fn server_errors_are_retried_under_one_delivery_id_each_attempt_signed() {
    let (receiver, url) = Receiver::scripted(&[Reply::Status(503), Reply::Status(503)]).await;
    let db = usage_store(&url, &["usage"], Some(SECRET)).await;
    pump_usage(&db, &quick_policy(), 3).await;

    let got = receiver.settle(3).await;
    assert_eq!(got.len(), 3, "503, 503, then 204");
    let id = got[0].header("x-modelrouter-delivery").unwrap();
    for d in &got {
        assert_eq!(d.header("x-modelrouter-delivery"), Some(id));
        assert_eq!(d.json()["delivery_id"], id);
        let ts = d.header("x-modelrouter-timestamp").unwrap();
        assert_eq!(d.header("x-modelrouter-signature"), Some(expected_signature(ts, &d.body).as_str()));
    }
}

#[tokio::test]
async fn each_attempt_carries_a_fresh_timestamp() {
    // Timestamps have one-second resolution, so the retry waits just over it.
    let policy = DeliveryPolicy {
        retry_delays: vec![Duration::from_millis(1100)],
        ..quick_policy()
    };
    let (receiver, url) = Receiver::scripted(&[Reply::Status(500)]).await;
    let db = usage_store(&url, &["usage"], Some(SECRET)).await;
    pump_usage(&db, &policy, 2).await;

    let got = receiver.wait_for(2).await;
    let (first, second) = (
        got[0].header("x-modelrouter-timestamp").unwrap().parse::<i64>().unwrap(),
        got[1].header("x-modelrouter-timestamp").unwrap().parse::<i64>().unwrap(),
    );
    assert!(second > first, "{first} then {second}");
    assert_ne!(got[0].header("x-modelrouter-signature"), got[1].header("x-modelrouter-signature"));
}

#[tokio::test]
async fn too_many_requests_is_retried() {
    let (receiver, url) = Receiver::scripted(&[Reply::Status(429)]).await;
    let db = usage_store(&url, &["usage"], Some(SECRET)).await;
    pump_usage(&db, &quick_policy(), 2).await;
    assert_eq!(receiver.settle(2).await.len(), 2);
}

#[tokio::test]
async fn a_client_error_is_not_retried() {
    let (receiver, url) = Receiver::scripted(&[Reply::Status(400)]).await;
    let db = usage_store(&url, &["usage"], Some(SECRET)).await;
    pump_usage(&db, &quick_policy(), 3).await;
    assert_eq!(receiver.settle(1).await.len(), 1);
}

#[tokio::test]
async fn retries_stop_when_the_schedule_runs_out() {
    let policy = DeliveryPolicy { retry_delays: vec![Duration::from_millis(10); 2], ..quick_policy() };
    let (receiver, url) = Receiver::scripted(&[Reply::Status(503); 5]).await;
    let db = usage_store(&url, &["usage"], Some(SECRET)).await;
    pump_usage(&db, &policy, 5).await;
    assert_eq!(receiver.settle(3).await.len(), 3, "one attempt plus two retries");
}

#[tokio::test]
async fn a_receiver_that_never_answers_times_out_and_is_retried() {
    let (receiver, url) = Receiver::scripted(&[Reply::Hang]).await;
    let db = usage_store(&url, &["usage"], Some(SECRET)).await;
    pump_usage(&db, &quick_policy(), 2).await;
    let got = receiver.wait_for(2).await;
    assert_eq!(got[0].header("x-modelrouter-delivery"), got[1].header("x-modelrouter-delivery"));
}

#[tokio::test]
async fn a_completion_delivery_times_out_and_is_not_retried() {
    let (receiver, url) = Receiver::scripted(&[Reply::Hang]).await;
    let b = WebhookBackend::with_policy(
        WebhookBackendConfig {
            name: "test".into(),
            url,
            events: vec!["completion".into()],
            secret_header_name: None,
            secret_header_value: None,
            signing_secret: None,
        },
        quick_policy(),
    );
    b.send(completion());
    // Long enough for the 200 ms timeout and any retry to have happened.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(receiver.seen.lock().unwrap().len(), 1);
}

/// Collects formatted log output so a test can assert on a warning.
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn deliveries_beyond_the_pending_bound_are_dropped_with_a_warning() {
    let logs = LogCapture::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (receiver, url) = Receiver::scripted(&[Reply::Hang]).await;
    let policy = DeliveryPolicy {
        timeout: Duration::from_secs(5),
        max_pending: 1,
        ..quick_policy()
    };
    let b = WebhookBackend::with_policy(
        WebhookBackendConfig {
            name: "test".into(),
            url,
            events: vec!["completion".into()],
            secret_header_name: None,
            secret_header_value: None,
            signing_secret: Some(SECRET.into()),
        },
        policy,
    );
    b.send(completion());
    b.send(completion());

    assert_eq!(receiver.settle(1).await.len(), 1, "the second delivery was dropped");
    let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(logged.contains("WARN"), "{logged}");
    assert!(logged.contains("webhook delivery dropped"), "{logged}");
    assert!(logged.contains("pending=1"), "{logged}");
}
