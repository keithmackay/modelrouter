//! Durable delivery of `usage` webhooks.
//!
//! [`enqueue_usage`] writes one `webhook_deliveries` row per subscribed
//! webhook immediately after the ledger insert. [`drain_once`] is what every
//! replica runs: it leases due rows and POSTs them. A replica that dies
//! mid-send keeps the row; the lease expires and another replica retries.
//! Delivery is at least once. The payload's `ledger_id` is the idempotency key.

use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use serde_json::Value;

use crate::db::models::CostLedgerEntry;
use crate::db::repositories::webhook_callbacks::WebhookCallbackRepository;
use crate::db::repositories::webhook_deliveries::{ClaimedDelivery, WebhookDeliveryRepository};

use super::usage;
use super::webhook::{self, post, DeliveryPolicy, WebhookBackendConfig};

/// How long a claim is held. Longer than one POST (the client times out at
/// 10 s) so a live attempt is not sent twice, and short enough that a dead
/// replica's row is picked up again promptly.
const LEASE: Duration = Duration::from_secs(30);

/// Delivered rows older than this are deleted. A row that was given up is kept.
const DELIVERED_RETENTION_DAYS: i64 = 7;

/// How many due rows one pass sends.
const BATCH: i64 = 16;

/// Wait before looking again at a delivery whose webhook is currently disabled.
const DISABLED_BACKOFF: Duration = Duration::from_secs(60);

/// What one [`drain_once`] pass did. Tests assert on it; the loop only logs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DrainReport {
    pub delivered: usize,
    pub retried: usize,
    pub abandoned: usize,
    pub released: usize,
}

/// The next step after one failed attempt. `attempts_already_made` is the
/// value stored before this try (0 for the first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetryChoice {
    Retry { attempt: i64, delay: Duration },
    GiveUp { attempt: i64 },
}

pub(crate) fn retry_choice(attempts_already_made: i64, retryable: bool) -> RetryChoice {
    let next = attempts_already_made.saturating_add(1);
    if !retryable {
        return RetryChoice::GiveUp { attempt: next };
    }
    match DeliveryPolicy::default()
        .retry_delays
        .get(attempts_already_made as usize)
        .copied()
    {
        Some(delay) => RetryChoice::Retry {
            attempt: next,
            delay,
        },
        None => RetryChoice::GiveUp { attempt: next },
    }
}

fn stamp(t: chrono::DateTime<Utc>) -> String {
    // Fixed width (`...Z`, no fractional seconds) so the text sort matches time.
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn clip_error(error: &str) -> String {
    let mut clipped: String = error.chars().take(500).collect();
    if error.chars().count() > 500 {
        clipped.push('…');
    }
    clipped
}

/// Insert one outbox row for each enabled webhook that subscribes to `usage`.
/// The stored payload is the webhook body, including a stable `delivery_id`.
pub async fn enqueue_usage<D>(db: &D, row: &CostLedgerEntry) -> anyhow::Result<usize>
where
    D: WebhookCallbackRepository + WebhookDeliveryRepository + ?Sized,
{
    let hooks = db.list_enabled_webhooks().await?;
    let due = stamp(Utc::now());
    let mut n = 0;
    for hook in hooks {
        if !webhook::subscribes_json(&hook.events, usage::EVENT) {
            continue;
        }
        let mut body = usage::from_ledger(row);
        body["delivery_id"] = Value::String(uuid::Uuid::new_v4().to_string());
        let payload = serde_json::to_string(&body)?;
        db.enqueue_delivery(hook.id, usage::EVENT, &payload, &due)
            .await?;
        n += 1;
    }
    Ok(n)
}

/// Claim and send one batch of due deliveries.
pub async fn drain_once<D>(db: &D, worker: &str, client: &reqwest::Client) -> DrainReport
where
    D: WebhookCallbackRepository + WebhookDeliveryRepository + ?Sized,
{
    let now = Utc::now();
    let claimed = match db
        .claim_due_deliveries(worker, &stamp(now), &stamp(now + LEASE), BATCH)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "webhook outbox claim failed");
            return DrainReport::default();
        }
    };
    if claimed.is_empty() {
        return DrainReport::default();
    }
    let hooks = match db.list_webhooks().await {
        Ok(hooks) => hooks,
        Err(e) => {
            // Leave the leases. They expire, and the rows are claimed again.
            tracing::warn!(error = %e, "webhook outbox could not read webhooks");
            return DrainReport::default();
        }
    };

    let mut report = DrainReport::default();
    for delivery in claimed {
        match send_one(db, client, worker, &hooks, delivery).await {
            OneOutcome::Delivered => report.delivered += 1,
            OneOutcome::Retried => report.retried += 1,
            OneOutcome::Abandoned => report.abandoned += 1,
            OneOutcome::Released => report.released += 1,
        }
    }
    report
}

/// Delete delivered rows older than [`DELIVERED_RETENTION_DAYS`].
pub async fn purge_delivered<D>(db: &D) -> anyhow::Result<u64>
where
    D: WebhookDeliveryRepository + ?Sized,
{
    let cutoff = stamp(Utc::now() - chrono::Duration::days(DELIVERED_RETENTION_DAYS));
    let deleted = db.purge_delivered_before(&cutoff).await?;
    if deleted > 0 {
        tracing::info!(deleted, "webhook outbox purged delivered rows");
    }
    Ok(deleted)
}

enum OneOutcome {
    Delivered,
    Retried,
    Abandoned,
    Released,
}

async fn send_one<D>(
    db: &D,
    client: &reqwest::Client,
    worker: &str,
    hooks: &[crate::db::repositories::webhook_callbacks::WebhookCallback],
    delivery: ClaimedDelivery,
) -> OneOutcome
where
    D: WebhookDeliveryRepository + ?Sized,
{
    let Some(hook) = hooks.iter().find(|h| h.id == delivery.webhook_id) else {
        return finish_abandon(db, worker, &delivery, "webhook removed").await;
    };
    if !hook.enabled {
        return finish_release(db, worker, &delivery, "webhook disabled").await;
    }
    if !webhook::subscribes_json(&hook.events, &delivery.event) {
        return finish_abandon(
            db,
            worker,
            &delivery,
            "webhook does not subscribe to this event",
        )
        .await;
    }
    let Ok(payload) = serde_json::from_str::<Value>(&delivery.payload) else {
        return finish_abandon(db, worker, &delivery, "payload is not json").await;
    };
    let delivery_id = payload
        .get("delivery_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let body = delivery.payload.as_bytes().to_vec();
    let config = WebhookBackendConfig::from_row(hook.clone());
    let failure = match post(client, &config, &delivery.event, &delivery_id, body).await {
        Ok(resp) if resp.status().is_success() => {
            let at = stamp(Utc::now());
            match db.mark_delivered(delivery.id, worker, &at).await {
                Ok(true) => return OneOutcome::Delivered,
                Ok(false) => {
                    tracing::warn!(
                        delivery_id = delivery.id,
                        "webhook POST succeeded after the lease was lost"
                    );
                    return OneOutcome::Delivered;
                }
                Err(e) => {
                    tracing::warn!(delivery_id = delivery.id, error = %e, "webhook mark delivered failed");
                    return OneOutcome::Delivered;
                }
            }
        }
        Ok(resp) if webhook::is_retryable(resp.status()) => format!("status {}", resp.status()),
        Ok(resp) => {
            let reason = format!("status {}", resp.status());
            return finish_abandon(db, worker, &delivery, &reason).await;
        }
        Err(e) => e.to_string(),
    };
    match retry_choice(delivery.attempt, true) {
        RetryChoice::Retry { attempt, delay } => {
            let at = stamp(Utc::now() + delay);
            let error = clip_error(&failure);
            match db
                .schedule_retry(delivery.id, worker, attempt, &at, &error)
                .await
            {
                Ok(true) => {
                    tracing::debug!(
                        webhook = config.name.as_str(),
                        delivery_id = delivery.id,
                        attempt,
                        "webhook POST failed ({failure}); retry scheduled in {delay:?}"
                    );
                    OneOutcome::Retried
                }
                Ok(false) => OneOutcome::Retried,
                Err(e) => {
                    tracing::warn!(delivery_id = delivery.id, error = %e, "webhook schedule retry failed");
                    OneOutcome::Retried
                }
            }
        }
        RetryChoice::GiveUp { attempt } => {
            tracing::warn!(
                webhook = config.name.as_str(),
                delivery_id = delivery.id,
                attempts = attempt,
                "webhook POST failed: {failure}"
            );
            finish_abandon(db, worker, &delivery, &clip_error(&failure)).await
        }
    }
}

async fn finish_abandon<D>(
    db: &D,
    worker: &str,
    delivery: &ClaimedDelivery,
    reason: &str,
) -> OneOutcome
where
    D: WebhookDeliveryRepository + ?Sized,
{
    let attempt = delivery.attempt.saturating_add(1);
    if let Err(e) = db
        .abandon_delivery(delivery.id, worker, attempt, &clip_error(reason))
        .await
    {
        tracing::warn!(delivery_id = delivery.id, error = %e, "webhook abandon failed");
    }
    OneOutcome::Abandoned
}

async fn finish_release<D>(
    db: &D,
    worker: &str,
    delivery: &ClaimedDelivery,
    reason: &str,
) -> OneOutcome
where
    D: WebhookDeliveryRepository + ?Sized,
{
    let at = stamp(Utc::now() + DISABLED_BACKOFF);
    if let Err(e) = db.release_delivery(delivery.id, worker, &at, reason).await {
        tracing::warn!(delivery_id = delivery.id, error = %e, "webhook release failed");
    }
    OneOutcome::Released
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callbacks::CallbackDispatcher;
    use crate::db::models::{CallKind, NewCostLedgerEntry, NewUser};
    use crate::db::repositories::costs::CostRepository;
    use crate::db::repositories::users::UserRepository;
    use crate::db::repositories::webhook_callbacks::{
        NewWebhookCallback, WebhookCallbackRepository,
    };
    use crate::db::sqlite::SqliteDb;

    fn choice_delay(attempts: i64) -> Duration {
        match retry_choice(attempts, true) {
            RetryChoice::Retry { delay, .. } => delay,
            RetryChoice::GiveUp { .. } => panic!("attempt {attempts} should retry"),
        }
    }

    #[test]
    fn usage_retries_follow_the_five_delays_and_then_stop() {
        let delays = [1, 5, 30, 120, 600].map(Duration::from_secs);
        for (attempt, delay) in delays.into_iter().enumerate() {
            assert_eq!(choice_delay(attempt as i64), delay);
        }
        assert_eq!(retry_choice(5, true), RetryChoice::GiveUp { attempt: 6 });
        assert_eq!(retry_choice(0, false), RetryChoice::GiveUp { attempt: 1 });
    }

    async fn migrated() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        crate::db::migrations::run_migrations(&db.pool)
            .await
            .unwrap();
        db
    }

    async fn user(db: &SqliteDb) -> i64 {
        UserRepository::create(
            db,
            NewUser {
                name: "alice".into(),
                email: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn webhook(db: &SqliteDb, name: &str, events: &str) -> i64 {
        WebhookCallbackRepository::create_webhook(
            db,
            NewWebhookCallback {
                name: name.into(),
                url: "http://127.0.0.1:9/hook".into(),
                events: events.into(),
                secret_header_name: None,
                secret_header_value: None,
                signing_secret: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    fn entry(user_id: i64) -> NewCostLedgerEntry {
        NewCostLedgerEntry {
            user_id,
            prompt_id: None,
            model: "m".into(),
            provider: "p".into(),
            project: None,
            tokens_in: 1,
            tokens_out: 1,
            cost_usd: 0.01,
            api_key_id: None,
            attribution_correlation_id: None,
            attribution_tags: "{}".into(),
            experiment_id: None,
            experiment_variant: None,
            tokens_estimated: false,
            call_kind: CallKind::Chat,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            request_fingerprint: None,
        }
    }

    #[derive(Debug, sqlx::FromRow)]
    struct Stored {
        id: i64,
        webhook_id: i64,
        attempt: i64,
        payload: String,
        next_attempt_at: Option<String>,
        locked_by: Option<String>,
        last_error: Option<String>,
        delivered_at: Option<String>,
    }

    async fn stored(db: &SqliteDb) -> Vec<Stored> {
        sqlx::query_as::<_, Stored>(
            "SELECT id, webhook_id, attempt, payload, next_attempt_at, locked_by, \
             last_error, delivered_at FROM webhook_deliveries ORDER BY id",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_ledger_row_enqueues_one_delivery_per_subscribed_webhook() {
        let db = migrated().await;
        let uid = user(&db).await;
        let usage_id = webhook(&db, "usage-sink", r#"["usage"]"#).await;
        let _completion = webhook(&db, "completion-sink", r#"["completion"]"#).await;
        let wildcard = webhook(&db, "all-sink", r#"["*"]"#).await;
        let disabled = webhook(&db, "off-sink", r#"["usage"]"#).await;
        db.set_webhook_enabled(disabled, false).await.unwrap();

        let row = CostRepository::create(&db, entry(uid)).await.unwrap();
        let n = enqueue_usage(&db, &row).await.unwrap();
        assert_eq!(n, 2);

        let rows = stored(&db).await;
        assert_eq!(rows.len(), 2);
        let hooks: Vec<i64> = rows.iter().map(|r| r.webhook_id).collect();
        assert!(hooks.contains(&usage_id));
        assert!(hooks.contains(&wildcard));
        for stored in &rows {
            assert_eq!(stored.attempt, 0);
            assert!(stored.next_attempt_at.is_some());
            assert!(stored.delivered_at.is_none());
            let payload: Value = serde_json::from_str(&stored.payload).unwrap();
            assert_eq!(payload["ledger_id"], row.id);
            assert_eq!(payload["event"], "usage");
            assert!(payload["delivery_id"].as_str().unwrap().len() > 8);
        }
    }

    #[tokio::test]
    async fn record_usage_enqueues_before_it_returns() {
        let db = migrated().await;
        let uid = user(&db).await;
        webhook(&db, "usage-sink", "[]").await;
        let callbacks = CallbackDispatcher::new(vec![]);
        let row = crate::api::ledger::record_usage(&db, &callbacks, entry(uid))
            .await
            .unwrap();
        let rows = stored(&db).await;
        assert_eq!(rows.len(), 1);
        let payload: Value = serde_json::from_str(&rows[0].payload).unwrap();
        assert_eq!(payload["ledger_id"], row.id);
    }

    #[tokio::test]
    async fn a_held_lease_is_not_claimed_until_it_expires() {
        let db = migrated().await;
        let id = db
            .enqueue_delivery(1, "usage", r#"{"event":"usage"}"#, &stamp(Utc::now()))
            .await
            .unwrap();
        let now = stamp(Utc::now());
        let until = stamp(Utc::now() + LEASE);
        let first = db
            .claim_due_deliveries("a", &now, &until, 10)
            .await
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, id);
        let second = db
            .claim_due_deliveries("b", &now, &until, 10)
            .await
            .unwrap();
        assert!(second.is_empty());

        sqlx::query("UPDATE webhook_deliveries SET locked_until = ? WHERE id = ?")
            .bind(stamp(Utc::now() - chrono::Duration::seconds(1)))
            .bind(id)
            .execute(&db.pool)
            .await
            .unwrap();
        let again = db
            .claim_due_deliveries("b", &stamp(Utc::now()), &stamp(Utc::now() + LEASE), 10)
            .await
            .unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].id, id);
        assert_eq!(again[0].attempt, 0);
    }

    #[tokio::test]
    async fn a_future_delivery_and_an_abandoned_one_are_not_due() {
        let db = migrated().await;
        db.enqueue_delivery(
            1,
            "usage",
            "{}",
            &stamp(Utc::now() + chrono::Duration::hours(1)),
        )
        .await
        .unwrap();
        let abandoned = db
            .enqueue_delivery(1, "usage", "{}", &stamp(Utc::now()))
            .await
            .unwrap();
        let now = stamp(Utc::now());
        let claimed = db
            .claim_due_deliveries("a", &now, &stamp(Utc::now() + LEASE), 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, abandoned);
        assert!(db
            .abandon_delivery(abandoned, "a", 1, "status 400")
            .await
            .unwrap());
        let none = db
            .claim_due_deliveries("a", &stamp(Utc::now()), &stamp(Utc::now() + LEASE), 10)
            .await
            .unwrap();
        assert!(none.is_empty());
        let rows = stored(&db).await;
        let given_up = rows.iter().find(|r| r.id == abandoned).unwrap();
        assert!(given_up.next_attempt_at.is_none());
        assert_eq!(given_up.last_error.as_deref(), Some("status 400"));
    }

    #[tokio::test]
    async fn delivered_rows_are_purged_and_pending_rows_are_kept() {
        let db = migrated().await;
        let pending = db
            .enqueue_delivery(1, "usage", "{}", &stamp(Utc::now()))
            .await
            .unwrap();
        let done = db
            .enqueue_delivery(1, "usage", "{}", &stamp(Utc::now()))
            .await
            .unwrap();
        let now = stamp(Utc::now());
        db.claim_due_deliveries("a", &now, &stamp(Utc::now() + LEASE), 10)
            .await
            .unwrap();
        // The pending row stays claimed by nobody after we only mark `done`.
        // Re-claim is unnecessary: mark the one we want and clear the other lock.
        assert!(db
            .mark_delivered(done, "a", "2020-01-01T00:00:00Z")
            .await
            .unwrap());
        sqlx::query(
            "UPDATE webhook_deliveries SET locked_by = NULL, locked_until = NULL WHERE id = ?",
        )
        .bind(pending)
        .execute(&db.pool)
        .await
        .unwrap();

        let deleted = db
            .purge_delivered_before("2020-01-02T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        let rows = stored(&db).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, pending);
    }

    /// Speaks just enough HTTP to answer the scripted status codes, in order.
    async fn scripted_hook(statuses: Vec<u16>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let statuses = std::sync::Arc::new(tokio::sync::Mutex::new(statuses));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let statuses = statuses.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let _ = sock.read(&mut buf).await;
                    let status = {
                        let mut codes = statuses.lock().await;
                        if codes.is_empty() {
                            200
                        } else {
                            codes.remove(0)
                        }
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}/hook")
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    async fn due_row(db: &SqliteDb, webhook_id: i64) {
        db.enqueue_delivery(
            webhook_id,
            "usage",
            r#"{"event":"usage","ledger_id":7,"delivery_id":"del-1"}"#,
            &stamp(Utc::now()),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_success_marks_the_row_delivered_and_a_retryable_failure_waits() {
        let db = migrated().await;
        let url = scripted_hook(vec![500, 200]).await;
        let hook = WebhookCallbackRepository::create_webhook(
            &db,
            NewWebhookCallback {
                name: "usage-sink".into(),
                url,
                events: r#"["usage"]"#.into(),
                secret_header_name: None,
                secret_header_value: None,
                signing_secret: Some("secret".into()),
            },
        )
        .await
        .unwrap();
        due_row(&db, hook.id).await;

        let report = drain_once(&db, "worker-a", &client()).await;
        assert_eq!(report.retried, 1);
        let rows = stored(&db).await;
        assert_eq!(rows[0].attempt, 1);
        assert!(rows[0].delivered_at.is_none());
        assert!(rows[0].locked_by.is_none());
        assert_eq!(
            rows[0].last_error.as_deref(),
            Some("status 500 Internal Server Error")
        );
        let next = rows[0].next_attempt_at.clone().unwrap();
        assert!(next.as_str() >= stamp(Utc::now() - chrono::Duration::seconds(1)).as_str());

        sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = ? WHERE id = ?")
            .bind(stamp(Utc::now() - chrono::Duration::seconds(1)))
            .bind(rows[0].id)
            .execute(&db.pool)
            .await
            .unwrap();
        let report = drain_once(&db, "worker-a", &client()).await;
        assert_eq!(report.delivered, 1);
        let rows = stored(&db).await;
        assert!(rows[0].delivered_at.is_some());
        assert!(rows[0].next_attempt_at.is_none());
        assert!(rows[0].last_error.is_none());
    }

    #[tokio::test]
    async fn a_client_error_is_not_retried() {
        let db = migrated().await;
        let url = scripted_hook(vec![400]).await;
        let hook = WebhookCallbackRepository::create_webhook(
            &db,
            NewWebhookCallback {
                name: "usage-sink".into(),
                url,
                events: r#"["usage"]"#.into(),
                secret_header_name: None,
                secret_header_value: None,
                signing_secret: None,
            },
        )
        .await
        .unwrap();
        due_row(&db, hook.id).await;
        let report = drain_once(&db, "worker-a", &client()).await;
        assert_eq!(report.abandoned, 1);
        let rows = stored(&db).await;
        assert!(rows[0].next_attempt_at.is_none());
        assert_eq!(
            rows[0].last_error.as_deref(),
            Some("status 400 Bad Request")
        );
        assert_eq!(rows[0].attempt, 1);
    }

    #[tokio::test]
    async fn a_removed_webhook_is_abandoned_and_a_disabled_one_is_released() {
        let db = migrated().await;
        let gone = db
            .enqueue_delivery(999, "usage", r#"{"delivery_id":"x"}"#, &stamp(Utc::now()))
            .await
            .unwrap();
        let hook = webhook(&db, "off-sink", r#"["usage"]"#).await;
        db.set_webhook_enabled(hook, false).await.unwrap();
        db.enqueue_delivery(hook, "usage", r#"{"delivery_id":"y"}"#, &stamp(Utc::now()))
            .await
            .unwrap();

        let report = drain_once(&db, "worker-a", &client()).await;
        assert_eq!(report.abandoned, 1);
        assert_eq!(report.released, 1);
        let rows = stored(&db).await;
        let removed = rows.iter().find(|r| r.id == gone).unwrap();
        assert!(removed.next_attempt_at.is_none());
        assert_eq!(removed.last_error.as_deref(), Some("webhook removed"));
        let held = rows.iter().find(|r| r.webhook_id == hook).unwrap();
        assert!(held.next_attempt_at.is_some());
        assert_eq!(held.attempt, 0);
        assert_eq!(held.last_error.as_deref(), Some("webhook disabled"));
    }
}
