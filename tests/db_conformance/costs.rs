//! Conformance scenarios: costs repositories.

// Expected rows are spelled out as tuples so a mismatch prints every column.
#![allow(clippy::type_complexity)]

#[allow(unused_imports)]
use crate::harness::each_backend;

use std::sync::Arc;
use std::time::Duration;

use modelrouter::api::app::DatabaseProvider;
use modelrouter::db::models::{CostLedgerEntry, NewApiKey, NewCostLedgerEntry, NewUser, RunStamp};
use modelrouter::db::repositories::costs::{
    ArmFilter, AttributionFilter, AttributionTotals, CostRepository,
};
use modelrouter::db::repositories::UserRepository;

type Db = Arc<dyn DatabaseProvider>;

// Ledger rows are written through `create`, which stamps `created_at` with
// the current time. Windows are therefore built from the timestamps the rows
// come back with, plus these open bounds.
const EPOCH: &str = "1970-01-01T00:00:00Z";
const FUTURE: &str = "9999-12-31T00:00:00Z";

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

async fn user(db: &Db, name: &str) -> i64 {
    UserRepository::create(&**db, NewUser { name: name.into(), email: None })
        .await
        .unwrap()
        .id
}

async fn api_key(db: &Db, user_id: i64, hash: &str, project: Option<&str>) -> i64 {
    db.create_api_key(NewApiKey {
        user_id,
        key_hash: hash.into(),
        label: None,
        expires_at: None,
        project: project.map(str::to_string),
        session_window_secs: None,
    })
    .await
    .unwrap()
    .id
}

/// A ledger entry with neutral defaults; scenarios override what they test.
fn entry(user_id: i64, model: &str, cost_usd: f64) -> NewCostLedgerEntry {
    NewCostLedgerEntry {
        call_kind: modelrouter::db::models::CallKind::Chat,
        user_id,
        prompt_id: None,
        model: model.into(),
        provider: "p".into(),
        project: None,
        tokens_in: 10,
        tokens_out: 5,
        cost_usd,
        api_key_id: None,
        attribution_correlation_id: None,
        attribution_tags: "{}".into(),
        experiment_id: None,
        experiment_variant: None,
        tokens_estimated: false,
    }
}

/// Write a row, then pause so the next row's `created_at` sorts after it.
async fn put(db: &Db, e: NewCostLedgerEntry) -> CostLedgerEntry {
    let row = CostRepository::create(&**db, e).await.unwrap();
    tokio::time::sleep(Duration::from_millis(3)).await;
    row
}

async fn put_hit(db: &Db, e: NewCostLedgerEntry) -> CostLedgerEntry {
    let row = db.create_cache_hit(e).await.unwrap();
    tokio::time::sleep(Duration::from_millis(3)).await;
    row
}

fn day(row: &CostLedgerEntry) -> String {
    row.created_at[..10].to_string()
}

fn run(id: &str) -> AttributionFilter {
    AttributionFilter::CorrelationId(id.into())
}

fn tag(key: &str, value: &str) -> AttributionFilter {
    AttributionFilter::Tag { key: key.into(), value: value.into() }
}

// ── Writes ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_round_trips_every_column() {
    each_backend(|db| async move {
        let u = user(&db, "alice").await;
        let k = api_key(&db, u, "h1", Some("proj")).await;
        let row = put(
            &db,
            NewCostLedgerEntry {
                provider: "prov".into(),
                project: Some("proj".into()),
                tokens_in: 12,
                tokens_out: 34,
                api_key_id: Some(k),
                attribution_correlation_id: Some("run-1".into()),
                attribution_tags: r#"{"arm":"a"}"#.into(),
                experiment_id: Some(4),
                experiment_variant: Some("control".into()),
                tokens_estimated: true,
                ..entry(u, "m1", 0.25)
            },
        )
        .await;
        assert!(row.id > 0);
        assert_eq!(row.user_id, u);
        assert_eq!(row.prompt_id, 0, "a NULL prompt id reads back as 0");
        assert_eq!((row.model.as_str(), row.provider.as_str()), ("m1", "prov"));
        assert_eq!(row.project.as_deref(), Some("proj"));
        assert_eq!((row.tokens_in, row.tokens_out), (12, 34));
        assert!(close(row.cost_usd, 0.25));
        assert!(!row.created_at.is_empty());
        assert_eq!(row.api_key_id, Some(k));
        assert!(!row.cache_hit);
        assert!(close(row.saved_usd, 0.0));
        assert_eq!(row.attribution_correlation_id.as_deref(), Some("run-1"));
        assert_eq!(row.attribution_tags, r#"{"arm":"a"}"#);
        assert_eq!(row.experiment_id, Some(4));
        assert_eq!(row.experiment_variant.as_deref(), Some("control"));
        assert!(row.tokens_estimated);

        let bare = put(&db, entry(u, "m2", 0.0)).await;
        assert!(bare.id > row.id);
        assert_eq!(bare.api_key_id, None);
        assert_eq!(bare.project, None);
        assert_eq!(bare.attribution_correlation_id, None);
        assert_eq!(bare.experiment_id, None);
        assert_eq!(bare.experiment_variant, None);
        assert!(!bare.tokens_estimated);
    })
    .await;
}

#[tokio::test]
async fn create_cache_hit_records_saving_not_spend() {
    each_backend(|db| async move {
        let u = user(&db, "alice").await;
        let hit = put_hit(
            &db,
            NewCostLedgerEntry {
                experiment_id: Some(4),
                experiment_variant: Some("control".into()),
                tokens_estimated: true,
                ..entry(u, "m1", 0.5)
            },
        )
        .await;
        assert!(hit.cache_hit);
        assert!(close(hit.cost_usd, 0.0), "a cache hit is never spend");
        assert!(close(hit.saved_usd, 0.5));
        assert_eq!(hit.prompt_id, 0);
        assert_eq!(hit.experiment_id, Some(4));
        assert!(hit.tokens_estimated);
        assert!(close(db.sum_global_since(EPOCH).await.unwrap(), 0.0));
    })
    .await;
}

// ── Cache aggregates ─────────────────────────────────────────────────────────

#[tokio::test]
async fn cache_summary_since_counts_hits_and_savings() {
    each_backend(|db| async move {
        let u = user(&db, "alice").await;
        let first = put(&db, entry(u, "m1", 1.0)).await;
        let second = put_hit(&db, entry(u, "m1", 2.0)).await;
        put_hit(&db, entry(u, "m2", 4.0)).await;

        let all = db.cache_summary_since(None, EPOCH).await.unwrap();
        assert_eq!((all.hits, all.requests), (2, 3));
        assert!(close(all.saved_usd, 6.0));
        assert!(close(all.hit_rate(), 2.0 / 3.0));

        let m1 = db.cache_summary_since(Some("m1"), EPOCH).await.unwrap();
        assert_eq!((m1.hits, m1.requests), (1, 2));
        assert!(close(m1.saved_usd, 2.0));

        // `since` is inclusive.
        let later = db.cache_summary_since(Some("m1"), &second.created_at).await.unwrap();
        assert_eq!((later.hits, later.requests), (1, 1));
        let from_first = db.cache_summary_since(None, &first.created_at).await.unwrap();
        assert_eq!(from_first.requests, 3);

        let none = db.cache_summary_since(None, FUTURE).await.unwrap();
        assert_eq!((none.hits, none.requests), (0, 0));
        assert!(close(none.saved_usd, 0.0));
    })
    .await;
}

#[tokio::test]
async fn cache_daily_series_groups_by_day_in_a_half_open_window() {
    each_backend(|db| async move {
        let u = user(&db, "alice").await;
        let first = put(&db, entry(u, "m1", 1.0)).await;
        put_hit(&db, entry(u, "m1", 1.0)).await;
        let last = put_hit(&db, entry(u, "m2", 1.0)).await;

        let series = db.cache_daily_series(EPOCH, FUTURE).await.unwrap();
        assert_eq!(series, vec![(day(&first), 2, 3)]);

        // Start inclusive, end exclusive.
        let windowed = db.cache_daily_series(&first.created_at, &last.created_at).await.unwrap();
        assert_eq!(windowed, vec![(day(&first), 1, 2)]);

        assert!(db.cache_daily_series(FUTURE, FUTURE).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn cache_summary_by_model_since_orders_by_hits_then_model() {
    each_backend(|db| async move {
        let u = user(&db, "alice").await;
        put(&db, entry(u, "b", 1.0)).await;
        put_hit(&db, entry(u, "b", 1.0)).await;
        put_hit(&db, entry(u, "c", 1.0)).await;
        put_hit(&db, entry(u, "c", 2.0)).await;
        put(&db, entry(u, "a", 1.0)).await;
        put_hit(&db, entry(u, "a", 0.5)).await;

        let rows = db.cache_summary_by_model_since(EPOCH).await.unwrap();
        let keys: Vec<(&str, i64, i64)> =
            rows.iter().map(|(m, s)| (m.as_str(), s.hits, s.requests)).collect();
        assert_eq!(keys, [("c", 2, 2), ("a", 1, 2), ("b", 1, 2)]);
        assert!(close(rows[0].1.saved_usd, 3.0));

        assert!(db.cache_summary_by_model_since(FUTURE).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn cache_rows_grouped_matches_the_cost_row_grouping() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        let k = api_key(&db, alice, "h1", Some("proj")).await;
        let keyed = |e: NewCostLedgerEntry| NewCostLedgerEntry {
            project: Some("proj".into()),
            api_key_id: Some(k),
            ..e
        };
        put(&db, keyed(entry(alice, "m1", 1.0))).await;
        put_hit(&db, keyed(entry(alice, "m1", 2.0))).await;
        put_hit(&db, entry(alice, "m2", 3.0)).await;
        put(&db, entry(bob, "m1", 4.0)).await;

        let mut rows = db.cache_rows_grouped(None, None, None, None, EPOCH).await.unwrap();
        rows.sort_by(|a, b| (a.0, &a.1, &a.2).cmp(&(b.0, &b.1, &b.2)));
        let keys: Vec<(i64, &str, Option<&str>, Option<i64>, i64, i64)> = rows
            .iter()
            .map(|r| (r.0, r.1.as_str(), r.2.as_deref(), r.3, r.4.hits, r.4.requests))
            .collect();
        assert_eq!(
            keys,
            [
                (alice, "m1", Some("proj"), Some(k), 1, 2),
                (alice, "m2", None, None, 1, 1),
                (bob, "m1", None, None, 0, 1),
            ]
        );
        let m1 = &rows[0].4;
        assert!(close(m1.saved_usd, 2.0));

        let by_project = db.cache_rows_grouped(None, Some("proj"), None, None, EPOCH).await.unwrap();
        assert_eq!(by_project.len(), 1);
        let by_key = db.cache_rows_grouped(None, None, Some(k), None, EPOCH).await.unwrap();
        assert_eq!(by_key.len(), 1);
        let by_model = db.cache_rows_grouped(None, None, None, Some("m1"), EPOCH).await.unwrap();
        assert_eq!(by_model.len(), 2);
        let by_user = db.cache_rows_grouped(Some(&[bob]), None, None, None, EPOCH).await.unwrap();
        assert_eq!(by_user.len(), 1);
        assert_eq!(by_user[0].0, bob);
        let all_filters = db
            .cache_rows_grouped(Some(&[alice, bob]), Some("proj"), Some(k), Some("m1"), EPOCH)
            .await
            .unwrap();
        assert_eq!(all_filters.len(), 1);
        assert_eq!(all_filters[0].4.hits, 1);

        assert!(db.cache_rows_grouped(Some(&[]), None, None, None, EPOCH).await.unwrap().is_empty());
        assert!(db.cache_rows_grouped(None, None, None, None, FUTURE).await.unwrap().is_empty());
    })
    .await;
}

// ── Spend sums ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn user_and_key_sums_since() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        let k1 = api_key(&db, alice, "h1", None).await;
        let k2 = api_key(&db, alice, "h2", None).await;
        let first = put(&db, NewCostLedgerEntry { api_key_id: Some(k1), ..entry(alice, "m", 1.0) }).await;
        let second = put(
            &db,
            NewCostLedgerEntry { api_key_id: Some(k1), tokens_in: 100, tokens_out: 50, ..entry(alice, "m", 2.0) },
        )
        .await;
        put(&db, NewCostLedgerEntry { api_key_id: Some(k2), ..entry(alice, "m", 4.0) }).await;
        put(&db, entry(bob, "m", 8.0)).await;
        // A cache hit adds tokens but no spend.
        put_hit(&db, NewCostLedgerEntry { api_key_id: Some(k1), ..entry(alice, "m", 16.0) }).await;

        assert!(close(db.sum_for_user_since(alice, EPOCH).await.unwrap(), 7.0));
        assert!(close(db.sum_for_user_since(alice, &second.created_at).await.unwrap(), 6.0));
        assert!(close(db.sum_for_user_since(bob, EPOCH).await.unwrap(), 8.0));
        assert!(close(db.sum_for_user_since(alice, FUTURE).await.unwrap(), 0.0));

        assert_eq!(db.sum_tokens_for_user_since(alice, EPOCH).await.unwrap(), 15 + 150 + 15 + 15);
        assert_eq!(db.sum_tokens_for_user_since(alice, &second.created_at).await.unwrap(), 150 + 15 + 15);
        assert_eq!(db.sum_tokens_for_user_since(alice, FUTURE).await.unwrap(), 0);

        assert!(close(db.sum_for_key_since(k1, EPOCH).await.unwrap(), 3.0));
        assert!(close(db.sum_for_key_since(k1, &second.created_at).await.unwrap(), 2.0));
        assert!(close(db.sum_for_key_since(k2, EPOCH).await.unwrap(), 4.0));
        assert!(close(db.sum_for_key_since(k1, FUTURE).await.unwrap(), 0.0));

        assert_eq!(db.sum_tokens_for_key_since(k1, EPOCH).await.unwrap(), 15 + 150 + 15);
        assert_eq!(db.sum_tokens_for_key_since(k1, &first.created_at).await.unwrap(), 15 + 150 + 15);
        assert_eq!(db.sum_tokens_for_key_since(k2, EPOCH).await.unwrap(), 15);
        assert_eq!(db.sum_tokens_for_key_since(k2, FUTURE).await.unwrap(), 0);
    })
    .await;
}

#[tokio::test]
async fn between_sums_use_a_half_open_window() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        let first = put(&db, NewCostLedgerEntry { project: Some("billing".into()), ..entry(alice, "m", 1.0) }).await;
        let second = put(&db, NewCostLedgerEntry { project: Some("billing".into()), ..entry(alice, "m", 2.0) }).await;
        let third = put(&db, NewCostLedgerEntry { project: Some("other".into()), ..entry(bob, "m", 4.0) }).await;
        let fourth = put(&db, NewCostLedgerEntry { project: Some("billing".into()), ..entry(bob, "m", 8.0) }).await;

        // [second, fourth): second and third only.
        let (s, e) = (second.created_at.as_str(), fourth.created_at.as_str());
        assert!(close(db.sum_for_user_between(alice, EPOCH, FUTURE).await.unwrap(), 3.0));
        assert!(close(db.sum_for_user_between(alice, s, e).await.unwrap(), 2.0));
        assert!(close(db.sum_for_user_between(alice, EPOCH, &first.created_at).await.unwrap(), 0.0));
        assert!(close(db.sum_for_user_between(bob, s, e).await.unwrap(), 4.0));

        assert!(close(db.sum_for_project_since("billing", EPOCH).await.unwrap(), 11.0));
        assert!(close(db.sum_for_project_since("billing", s).await.unwrap(), 10.0));
        assert!(close(db.sum_for_project_since("missing", EPOCH).await.unwrap(), 0.0));

        assert!(close(db.sum_for_project_between("billing", EPOCH, FUTURE).await.unwrap(), 11.0));
        assert!(close(db.sum_for_project_between("billing", s, e).await.unwrap(), 2.0));
        assert!(close(db.sum_for_project_between("other", s, e).await.unwrap(), 4.0));

        assert!(close(db.sum_global_since(EPOCH).await.unwrap(), 15.0));
        assert!(close(db.sum_global_since(&third.created_at).await.unwrap(), 12.0));
        assert!(close(db.sum_global_since(FUTURE).await.unwrap(), 0.0));

        assert!(close(db.sum_global_between(EPOCH, FUTURE).await.unwrap(), 15.0));
        assert!(close(db.sum_global_between(s, e).await.unwrap(), 6.0));
        assert!(close(db.sum_global_between(s, s).await.unwrap(), 0.0));
    })
    .await;
}

#[tokio::test]
async fn user_cost_stats_since_totals_one_user() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        put(&db, entry(alice, "m", 1.0)).await;
        let second = put(&db, NewCostLedgerEntry { tokens_in: 7, tokens_out: 3, ..entry(alice, "m", 2.0) }).await;
        put_hit(&db, entry(alice, "m", 5.0)).await;
        put(&db, entry(bob, "m", 100.0)).await;

        let (cost, ti, to, n) = db.user_cost_stats_since(alice, EPOCH).await.unwrap();
        assert!(close(cost, 3.0));
        assert_eq!((ti, to, n), (27, 13, 3));

        let (cost, ti, to, n) = db.user_cost_stats_since(alice, &second.created_at).await.unwrap();
        assert!(close(cost, 2.0));
        assert_eq!((ti, to, n), (17, 8, 2));

        let (cost, ti, to, n) = db.user_cost_stats_since(alice, FUTURE).await.unwrap();
        assert!(close(cost, 0.0));
        assert_eq!((ti, to, n), (0, 0, 0));
    })
    .await;
}

#[tokio::test]
async fn cost_stats_grouped_by_user_with_filters() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        let carol = user(&db, "carol").await;
        let k = api_key(&db, alice, "h1", Some("proj")).await;
        put(
            &db,
            NewCostLedgerEntry { project: Some("proj".into()), api_key_id: Some(k), ..entry(alice, "m", 1.0) },
        )
        .await;
        put(&db, entry(alice, "m", 2.0)).await;
        put_hit(&db, entry(alice, "m", 4.0)).await;
        let late = put(&db, entry(bob, "m", 8.0)).await;
        // Carol has only a cache hit: zero spend, but still a row.
        put_hit(&db, entry(carol, "m", 0.5)).await;

        let mut rows = db.cost_stats_grouped(None, None, None, EPOCH).await.unwrap();
        rows.sort_by_key(|r| r.0);
        assert_eq!(rows.len(), 3);
        let (uid, cost, ti, to, n, uncached) = rows[0];
        assert_eq!(uid, alice);
        assert!(close(cost, 3.0));
        assert_eq!((ti, to, n), (30, 15, 3));
        assert!(close(uncached, 7.0));
        assert_eq!(rows[1].0, bob);
        assert!(close(rows[1].1, 8.0));
        let (uid, cost, _, _, n, uncached) = rows[2];
        assert_eq!((uid, n), (carol, 1));
        assert!(close(cost, 0.0));
        assert!(close(uncached, 0.5));

        let proj = db.cost_stats_grouped(None, Some("proj"), None, EPOCH).await.unwrap();
        assert_eq!(proj.len(), 1);
        assert!(close(proj[0].1, 1.0));
        let keyed = db.cost_stats_grouped(None, None, Some(k), EPOCH).await.unwrap();
        assert_eq!(keyed.len(), 1);
        assert_eq!(keyed[0].4, 1);
        let both = db.cost_stats_grouped(Some(&[alice]), Some("proj"), Some(k), EPOCH).await.unwrap();
        assert_eq!(both.len(), 1);
        let only_bob = db.cost_stats_grouped(Some(&[bob, carol]), None, None, EPOCH).await.unwrap();
        assert_eq!(only_bob.len(), 2);
        let since_late = db.cost_stats_grouped(None, None, None, &late.created_at).await.unwrap();
        assert_eq!(since_late.len(), 2);

        assert!(db.cost_stats_grouped(Some(&[]), None, None, EPOCH).await.unwrap().is_empty());
        assert!(db.cost_stats_grouped(None, None, None, FUTURE).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn cost_rows_grouped_highest_spend_first() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        let k = api_key(&db, alice, "h1", Some("proj")).await;
        let keyed = |e: NewCostLedgerEntry| NewCostLedgerEntry {
            project: Some("proj".into()),
            api_key_id: Some(k),
            ..e
        };
        put(&db, keyed(entry(alice, "m1", 1.0))).await;
        put(&db, keyed(entry(alice, "m1", 2.0))).await;
        put(&db, entry(alice, "m2", 8.0)).await;
        let late = put(&db, entry(bob, "m1", 4.0)).await;
        put_hit(&db, entry(bob, "m3", 9.0)).await;

        let rows = db.cost_rows_grouped(None, None, None, None, EPOCH).await.unwrap();
        let keys: Vec<(i64, &str, Option<&str>, Option<i64>, i64, i64, i64)> = rows
            .iter()
            .map(|r| (r.0, r.1.as_str(), r.2.as_deref(), r.3, r.5, r.6, r.7))
            .collect();
        assert_eq!(
            keys,
            [
                (alice, "m2", None, None, 10, 5, 1),
                (bob, "m1", None, None, 10, 5, 1),
                (alice, "m1", Some("proj"), Some(k), 20, 10, 2),
                (bob, "m3", None, None, 10, 5, 1),
            ]
        );
        assert!(close(rows[0].4, 8.0));
        assert!(close(rows[2].4, 3.0));
        assert!(close(rows[3].4, 0.0));

        assert_eq!(db.cost_rows_grouped(None, Some("proj"), None, None, EPOCH).await.unwrap().len(), 1);
        assert_eq!(db.cost_rows_grouped(None, None, Some(k), None, EPOCH).await.unwrap().len(), 1);
        assert_eq!(db.cost_rows_grouped(None, None, None, Some("m1"), EPOCH).await.unwrap().len(), 2);
        assert_eq!(db.cost_rows_grouped(Some(&[bob]), None, None, None, EPOCH).await.unwrap().len(), 2);
        let all = db
            .cost_rows_grouped(Some(&[alice]), Some("proj"), Some(k), Some("m1"), EPOCH)
            .await
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].7, 2);
        assert_eq!(db.cost_rows_grouped(None, None, None, None, &late.created_at).await.unwrap().len(), 2);

        assert!(db.cost_rows_grouped(Some(&[]), None, None, None, EPOCH).await.unwrap().is_empty());
        assert!(db.cost_rows_grouped(None, None, None, None, FUTURE).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn list_daily_spend_groups_by_day_with_filters() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        let first = put(&db, NewCostLedgerEntry { project: Some("proj".into()), ..entry(alice, "m1", 1.0) }).await;
        put(&db, entry(alice, "m2", 2.0)).await;
        let last = put(&db, entry(bob, "m1", 4.0)).await;
        let d = day(&first);

        assert_eq!(db.list_daily_spend(None, None, None, EPOCH, FUTURE).await.unwrap(), vec![(d.clone(), 7.0)]);
        assert_eq!(
            db.list_daily_spend(None, Some("proj"), None, EPOCH, FUTURE).await.unwrap(),
            vec![(d.clone(), 1.0)]
        );
        assert_eq!(
            db.list_daily_spend(None, None, Some("m1"), EPOCH, FUTURE).await.unwrap(),
            vec![(d.clone(), 5.0)]
        );
        assert_eq!(
            db.list_daily_spend(Some(&[alice]), None, Some("m2"), EPOCH, FUTURE).await.unwrap(),
            vec![(d.clone(), 2.0)]
        );
        assert_eq!(
            db.list_daily_spend(Some(&[alice]), Some("proj"), Some("m1"), EPOCH, FUTURE).await.unwrap(),
            vec![(d.clone(), 1.0)]
        );
        // Start inclusive, end exclusive.
        assert_eq!(
            db.list_daily_spend(None, None, None, &first.created_at, &last.created_at).await.unwrap(),
            vec![(d.clone(), 3.0)]
        );

        assert!(db.list_daily_spend(Some(&[]), None, None, EPOCH, FUTURE).await.unwrap().is_empty());
        assert!(db.list_daily_spend(None, None, None, FUTURE, FUTURE).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn summarize_by_model_highest_spend_first() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        put(&db, NewCostLedgerEntry { project: Some("proj".into()), ..entry(alice, "m1", 1.0) }).await;
        put(&db, entry(bob, "m1", 2.0)).await;
        let late = put(&db, entry(alice, "m2", 8.0)).await;
        put_hit(&db, entry(alice, "m2", 16.0)).await;

        let rows = db.summarize_by_model(None, None, None, EPOCH).await.unwrap();
        let keys: Vec<(&str, i64, i64, i64)> = rows
            .iter()
            .map(|r| (r.model.as_str(), r.tokens_in, r.tokens_out, r.request_count))
            .collect();
        assert_eq!(keys, [("m2", 20, 10, 2), ("m1", 20, 10, 2)]);
        assert!(close(rows[0].total_cost_usd, 8.0));
        assert!(close(rows[0].uncached_cost_usd, 24.0));
        assert!(close(rows[1].total_cost_usd, 3.0));
        assert!(close(rows[1].uncached_cost_usd, 3.0));

        let proj = db.summarize_by_model(None, Some("proj"), None, EPOCH).await.unwrap();
        assert_eq!(proj.len(), 1);
        assert!(close(proj[0].total_cost_usd, 1.0));
        let m1 = db.summarize_by_model(None, None, Some("m1"), EPOCH).await.unwrap();
        assert_eq!(m1.len(), 1);
        let bob_only = db.summarize_by_model(Some(&[bob]), None, None, EPOCH).await.unwrap();
        assert_eq!(bob_only.len(), 1);
        assert!(close(bob_only[0].total_cost_usd, 2.0));
        let all = db.summarize_by_model(Some(&[alice]), Some("proj"), Some("m1"), EPOCH).await.unwrap();
        assert_eq!(all.len(), 1);
        let since = db.summarize_by_model(None, None, None, &late.created_at).await.unwrap();
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].model, "m2");

        assert!(db.summarize_by_model(Some(&[]), None, None, EPOCH).await.unwrap().is_empty());
        assert!(db.summarize_by_model(None, None, None, FUTURE).await.unwrap().is_empty());
    })
    .await;
}

// ── Retention ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_before_and_delete_by_ids() {
    each_backend(|db| async move {
        let u = user(&db, "alice").await;
        let a = put(&db, entry(u, "m1", 1.0)).await;
        let b = put_hit(&db, entry(u, "m2", 2.0)).await;
        let c = put(&db, entry(u, "m3", 4.0)).await;

        let before_c = db.list_cost_entries_before(&c.created_at).await.unwrap();
        let ids: Vec<i64> = before_c.iter().map(|r| r.id).collect();
        assert_eq!(ids, [a.id, b.id], "oldest first, cutoff exclusive");
        assert!(before_c[1].cache_hit);
        assert!(close(before_c[1].saved_usd, 2.0));
        assert_eq!(before_c[0].model, "m1");
        assert!(db.list_cost_entries_before(EPOCH).await.unwrap().is_empty());
        assert_eq!(db.list_cost_entries_before(FUTURE).await.unwrap().len(), 3);

        db.delete_cost_entries_by_ids(&[]).await.unwrap();
        assert_eq!(db.list_cost_entries_before(FUTURE).await.unwrap().len(), 3);
        db.delete_cost_entries_by_ids(&[a.id, c.id, 999_999]).await.unwrap();
        let left: Vec<i64> =
            db.list_cost_entries_before(FUTURE).await.unwrap().iter().map(|r| r.id).collect();
        assert_eq!(left, [b.id]);
        assert!(close(db.sum_global_since(EPOCH).await.unwrap(), 0.0));
    })
    .await;
}

// ── Pickers ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn distinct_projects_models_and_providers_are_sorted_and_deduped() {
    each_backend(|db| async move {
        let u = user(&db, "alice").await;
        assert!(db.distinct_projects_in_ledger().await.unwrap().is_empty());
        assert!(db.distinct_models_in_ledger().await.unwrap().is_empty());
        assert!(db.distinct_providers_in_ledger().await.unwrap().is_empty());

        let row = |model: &str, provider: &str, project: Option<&str>| NewCostLedgerEntry {
            provider: provider.into(),
            project: project.map(str::to_string),
            ..entry(u, model, 1.0)
        };
        put(&db, row("zeta-model", "zeta", Some("zproj"))).await;
        put(&db, row("alpha-model", "alpha", Some("aproj"))).await;
        put(&db, row("alpha-model", "alpha", None)).await;
        put_hit(&db, row("mid-model", "alpha", Some("aproj"))).await;

        assert_eq!(db.distinct_projects_in_ledger().await.unwrap(), ["aproj", "zproj"]);
        assert_eq!(db.distinct_models_in_ledger().await.unwrap(), ["alpha-model", "mid-model", "zeta-model"]);
        assert_eq!(db.distinct_providers_in_ledger().await.unwrap(), ["alpha", "zeta"]);
    })
    .await;
}

#[tokio::test]
async fn distinct_recent_correlation_ids_newest_first_and_capped() {
    each_backend(|db| async move {
        let u = user(&db, "alice").await;
        let with = |id: Option<&str>| NewCostLedgerEntry {
            attribution_correlation_id: id.map(str::to_string),
            ..entry(u, "m", 1.0)
        };
        put(&db, with(Some("old"))).await;
        put(&db, with(Some("new"))).await;
        put(&db, with(Some("mid"))).await;
        put(&db, with(Some("old"))).await;
        put(&db, with(Some("new"))).await;
        put(&db, with(None)).await;
        put(&db, with(Some(""))).await;

        assert_eq!(db.distinct_recent_correlation_ids(2).await.unwrap(), ["new", "old"]);
        assert_eq!(db.distinct_recent_correlation_ids(10).await.unwrap(), ["new", "old", "mid"]);
        assert!(db.distinct_recent_correlation_ids(0).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn distinct_attribution_tag_keys_and_values() {
    each_backend(|db| async move {
        let u = user(&db, "alice").await;
        assert!(db.distinct_attribution_tag_keys().await.unwrap().is_empty());
        let with = |run: Option<&str>, tags: &str| NewCostLedgerEntry {
            attribution_correlation_id: run.map(str::to_string),
            attribution_tags: tags.into(),
            ..entry(u, "m", 1.0)
        };
        put(&db, with(Some("r2"), r#"{"team":"b","arm":"x"}"#)).await;
        put(&db, with(Some("r1"), r#"{"team":"a"}"#)).await;
        put(&db, with(Some("r2"), r#"{"team":"a"}"#)).await;
        put(&db, with(None, "{}")).await;
        put_hit(&db, with(Some(""), r#"{"zone":"c"}"#)).await;

        assert_eq!(db.distinct_attribution_tag_keys().await.unwrap(), ["arm", "team", "zone"]);

        assert_eq!(db.distinct_attribution_values(Some("team"), 10).await.unwrap(), ["a", "b"]);
        assert_eq!(db.distinct_attribution_values(Some("team"), 1).await.unwrap(), ["a"]);
        assert_eq!(db.distinct_attribution_values(Some("arm"), 10).await.unwrap(), ["x"]);
        assert!(db.distinct_attribution_values(Some("missing"), 10).await.unwrap().is_empty());
        // No key: correlation ids, empty string included.
        assert_eq!(db.distinct_attribution_values(None, 10).await.unwrap(), ["", "r1", "r2"]);
        assert_eq!(db.distinct_attribution_values(None, 2).await.unwrap(), ["", "r1"]);
    })
    .await;
}

// ── Experiment stamps ────────────────────────────────────────────────────────

#[tokio::test]
async fn run_stamp_is_the_earliest_stamped_row() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        let stamped = |uid: i64, run: &str, exp: Option<(i64, &str)>| NewCostLedgerEntry {
            attribution_correlation_id: Some(run.into()),
            experiment_id: exp.map(|(id, _)| id),
            experiment_variant: exp.map(|(_, v)| v.to_string()),
            ..entry(uid, "m", 0.0)
        };
        // An unstamped row first; the earliest *stamped* row wins.
        put(&db, stamped(alice, "run-1", None)).await;
        put(&db, stamped(alice, "run-1", Some((7, "control")))).await;
        put(&db, stamped(alice, "run-1", Some((7, "candidate")))).await;
        // The same correlation id under another user is a different run.
        put(&db, stamped(bob, "run-1", Some((8, "other")))).await;
        put(&db, stamped(alice, "run-2", None)).await;

        assert_eq!(
            db.run_stamp(alice, "run-1").await.unwrap(),
            Some(RunStamp { experiment_id: Some(7), experiment_variant: Some("control".into()) })
        );
        assert_eq!(db.run_stamp(bob, "run-1").await.unwrap().unwrap().experiment_id, Some(8));
        assert_eq!(db.run_stamp(alice, "run-2").await.unwrap(), Some(RunStamp::default()));
        assert_eq!(db.run_stamp(alice, "run-3").await.unwrap(), None);
        assert_eq!(db.run_stamp(bob, "run-2").await.unwrap(), None);
    })
    .await;
}

// ── Comparison arms and attribution ──────────────────────────────────────────

/// Rows for the arm scenarios. Returns the created rows in order.
async fn seed_arms(db: &Db, u: i64) -> Vec<CostLedgerEntry> {
    let row = |model: &str, provider: &str, run: Option<&str>, tags: &str, exp: Option<(i64, &str)>, cost: f64| {
        NewCostLedgerEntry {
            provider: provider.into(),
            attribution_correlation_id: run.map(str::to_string),
            attribution_tags: tags.into(),
            experiment_id: exp.map(|(id, _)| id),
            experiment_variant: exp.map(|(_, v)| v.to_string()),
            ..entry(u, model, cost)
        }
    };
    vec![
        put(db, row("X", "p1", Some("run-1"), r#"{"arm":"a"}"#, Some((7, "control")), 1.0)).await,
        put_hit(db, row("X", "p1", Some("run-1"), r#"{"arm":"a","other":"z"}"#, Some((7, "control")), 2.0)).await,
        put(db, row("X", "p2", Some("run-10"), r#"{"arm":"b"}"#, Some((7, "candidate")), 4.0)).await,
        put(db, row("Y", "p1", Some("run-1"), "{}", Some((8, "control")), 8.0)).await,
        put(db, row("Y", "p2", None, r#"{"other":"a"}"#, None, 16.0)).await,
        put(db, row("Y", "p1", Some("run-1"), r#"{"arm":"a"}"#, Some((7, "control")), 32.0)).await,
    ]
}

#[tokio::test]
async fn arm_totals_for_every_arm_kind() {
    each_backend(|db| async move {
        let u = user(&db, "alice").await;
        let rows = seed_arms(&db, u).await;

        let x = db.arm_totals(&ArmFilter::Model("X".into()), EPOCH, FUTURE).await.unwrap();
        assert_eq!(
            x,
            AttributionTotals {
                cost_usd: 5.0,
                saved_usd: 2.0,
                uncached_cost_usd: 7.0,
                tokens_in: 30,
                tokens_out: 15,
                requests: 3,
                cache_hits: 1,
            }
        );
        assert!(close(x.hit_rate(), 1.0 / 3.0));

        let p1 = db.arm_totals(&ArmFilter::Provider("p1".into()), EPOCH, FUTURE).await.unwrap();
        assert_eq!((p1.requests, p1.cache_hits), (4, 1));
        assert!(close(p1.cost_usd, 41.0));

        let a = db.arm_totals(&ArmFilter::Attribution(tag("arm", "a")), EPOCH, FUTURE).await.unwrap();
        assert_eq!(a.requests, 3);
        assert!(close(a.cost_usd, 33.0));
        let b = db.arm_totals(&ArmFilter::Attribution(tag("arm", "b")), EPOCH, FUTURE).await.unwrap();
        assert_eq!(b.requests, 1);

        // Correlation ids match exactly, not by prefix.
        let r1 = db.arm_totals(&ArmFilter::Attribution(run("run-1")), EPOCH, FUTURE).await.unwrap();
        assert_eq!(r1.requests, 4);
        assert!(close(r1.cost_usd, 41.0));

        let control = db
            .arm_totals(&ArmFilter::Variant { experiment_id: 7, variant: "control".into() }, EPOCH, FUTURE)
            .await
            .unwrap();
        assert_eq!(control.requests, 3);
        assert!(close(control.cost_usd, 33.0));
        assert!(close(control.saved_usd, 2.0));

        // Half-open window over the rows' own timestamps: [rows[1], rows[3]).
        let windowed = db
            .arm_totals(&ArmFilter::Model("X".into()), &rows[1].created_at, &rows[3].created_at)
            .await
            .unwrap();
        assert_eq!(windowed.requests, 2);
        assert!(close(windowed.cost_usd, 4.0));

        let none = db.arm_totals(&ArmFilter::Model("Z".into()), EPOCH, FUTURE).await.unwrap();
        assert_eq!(none, AttributionTotals::default());
    })
    .await;
}

#[tokio::test]
async fn arm_by_model_and_by_day() {
    each_backend(|db| async move {
        let u = user(&db, "alice").await;
        let rows = seed_arms(&db, u).await;

        let by_model = db.arm_by_model(&ArmFilter::Attribution(run("run-1")), EPOCH, FUTURE).await.unwrap();
        let keys: Vec<(&str, i64)> = by_model.iter().map(|r| (r.key.as_str(), r.totals.requests)).collect();
        assert_eq!(keys, [("Y", 2), ("X", 2)], "highest spend first");
        assert!(close(by_model[0].totals.cost_usd, 40.0));
        assert!(close(by_model[1].totals.saved_usd, 2.0));
        assert!(close(by_model[1].totals.uncached_cost_usd, 3.0));
        assert_eq!(by_model[1].totals.cache_hits, 1);

        let p2 = db.arm_by_model(&ArmFilter::Provider("p2".into()), EPOCH, FUTURE).await.unwrap();
        let keys: Vec<&str> = p2.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["Y", "X"]);
        assert!(db.arm_by_model(&ArmFilter::Model("Z".into()), EPOCH, FUTURE).await.unwrap().is_empty());

        let by_day = db.arm_by_day(&ArmFilter::Model("X".into()), EPOCH, FUTURE).await.unwrap();
        assert_eq!(by_day.len(), 1);
        assert_eq!(by_day[0].key, day(&rows[0]));
        assert_eq!(by_day[0].totals.requests, 3);
        assert!(close(by_day[0].totals.cost_usd, 5.0));
        let windowed = db
            .arm_by_day(&ArmFilter::Model("X".into()), &rows[0].created_at, &rows[2].created_at)
            .await
            .unwrap();
        assert_eq!(windowed[0].totals.requests, 2);
        assert!(db.arm_by_day(&ArmFilter::Model("X".into()), FUTURE, FUTURE).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn attribution_queries_delegate_to_the_arms() {
    each_backend(|db| async move {
        let u = user(&db, "alice").await;
        let rows = seed_arms(&db, u).await;

        for filter in [run("run-1"), run("run-10"), tag("arm", "a"), tag("other", "a"), run("none")] {
            let arm = ArmFilter::Attribution(filter.clone());
            assert_eq!(
                db.attribution_totals(&filter, EPOCH, FUTURE).await.unwrap(),
                db.arm_totals(&arm, EPOCH, FUTURE).await.unwrap()
            );
            assert_eq!(
                db.attribution_by_model(&filter, EPOCH, FUTURE).await.unwrap(),
                db.arm_by_model(&arm, EPOCH, FUTURE).await.unwrap()
            );
            assert_eq!(
                db.attribution_by_day(&filter, EPOCH, FUTURE).await.unwrap(),
                db.arm_by_day(&arm, EPOCH, FUTURE).await.unwrap()
            );
        }

        let other = db.attribution_totals(&tag("other", "a"), EPOCH, FUTURE).await.unwrap();
        assert_eq!(other.requests, 1);
        assert!(close(other.cost_usd, 16.0));
        let z = db.attribution_totals(&tag("other", "z"), EPOCH, FUTURE).await.unwrap();
        assert_eq!((z.requests, z.cache_hits), (1, 1));
        assert!(close(z.uncached_cost_usd, 2.0));

        let by_day = db.attribution_by_day(&run("run-1"), EPOCH, FUTURE).await.unwrap();
        assert_eq!(by_day.len(), 1);
        assert_eq!(by_day[0].key, day(&rows[0]));
        assert_eq!(by_day[0].totals.requests, 4);
    })
    .await;
}

// ── Experiment results ───────────────────────────────────────────────────────

/// Experiment 7 (rows written in this order): run `b` under bob, control;
/// run `a` mixed (control first, then candidate); run `b` candidate with an
/// estimated row and an unbound turn; unrelated rows for experiment 8 and an
/// unstamped run.
async fn seed_experiment(db: &Db) -> (i64, i64, Vec<CostLedgerEntry>) {
    let alice = user(db, "alice").await;
    let bob = user(db, "bob").await;
    let specs: [(i64, &str, Option<(i64, &str)>, &str, f64, (i64, i64), bool); 7] = [
        (bob, "b", Some((7, "control")), "m1", 0.08, (80, 40), false),
        (alice, "a", Some((7, "control")), "m1", 0.01, (10, 5), false),
        (alice, "a", Some((7, "candidate")), "m2", 0.02, (20, 10), false),
        (alice, "b", Some((7, "candidate")), "m2", 0.04, (40, 20), true),
        (alice, "b", None, "m2", 9.0, (900, 900), false),
        (alice, "z", Some((8, "control")), "m1", 5.0, (1, 1), false),
        (alice, "none", None, "m1", 5.0, (1, 1), false),
    ];
    let mut rows = Vec::new();
    for (uid, run, exp, model, cost, (ti, to), estimated) in specs {
        rows.push(
            put(
                db,
                NewCostLedgerEntry {
                    tokens_in: ti,
                    tokens_out: to,
                    attribution_correlation_id: Some(run.into()),
                    experiment_id: exp.map(|(id, _)| id),
                    experiment_variant: exp.map(|(_, v)| v.to_string()),
                    tokens_estimated: estimated,
                    ..entry(uid, model, cost)
                },
            )
            .await,
        );
    }
    (alice, bob, rows)
}

#[tokio::test]
async fn experiment_variant_totals_follow_each_rows_variant() {
    each_backend(|db| async move {
        seed_experiment(&db).await;
        let totals = db.experiment_variant_totals(7).await.unwrap();
        let labels: Vec<&str> = totals.iter().map(|t| t.variant.as_str()).collect();
        assert_eq!(labels, ["candidate", "control"]);
        let candidate = &totals[0];
        assert_eq!(candidate.requests, 2);
        assert!(close(candidate.cost_usd, 0.06));
        assert!(close(candidate.saved_usd, 0.0));
        assert_eq!((candidate.tokens_in, candidate.tokens_out), (60, 30));
        assert_eq!(candidate.estimated_rows, 1);
        let control = &totals[1];
        assert_eq!(control.requests, 2);
        assert!(close(control.cost_usd, 0.09));
        assert_eq!((control.tokens_in, control.tokens_out), (90, 45));
        assert_eq!(control.estimated_rows, 0);
        assert!(db.experiment_variant_totals(9).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn experiment_variant_models_split_by_model() {
    each_backend(|db| async move {
        let (alice, _, _) = seed_experiment(&db).await;
        // A second control model, cheaper than m1, sorts after it.
        put(
            &db,
            NewCostLedgerEntry {
                attribution_correlation_id: Some("c".into()),
                experiment_id: Some(7),
                experiment_variant: Some("control".into()),
                ..entry(alice, "m0", 0.001)
            },
        )
        .await;
        let rows = db.experiment_variant_models(7).await.unwrap();
        let keys: Vec<(&str, &str, i64)> =
            rows.iter().map(|r| (r.variant.as_str(), r.model.as_str(), r.requests)).collect();
        assert_eq!(keys, [("candidate", "m2", 2), ("control", "m1", 2), ("control", "m0", 1)]);
        assert_eq!(rows[0].estimated_rows, 1);
        assert_eq!((rows[0].tokens_in, rows[0].tokens_out), (60, 30));
        assert!(close(rows[1].cost_usd, 0.09));
        assert!(close(rows[1].saved_usd, 0.0));
        assert!(db.experiment_variant_models(9).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn experiment_runs_take_the_earliest_variant_and_flag_mixed() {
    each_backend(|db| async move {
        let (alice, bob, rows) = seed_experiment(&db).await;

        let runs = db.experiment_runs(7, 10, 0).await.unwrap();
        let order: Vec<(i64, &str)> = runs.iter().map(|r| (r.user_id, r.correlation_id.as_str())).collect();
        assert_eq!(order, [(alice, "b"), (alice, "a"), (bob, "b")], "last stamped activity first");

        let a = &runs[1];
        assert_eq!(a.variant, "control", "earliest stamped row wins");
        assert!(a.mixed());
        assert_eq!(a.variant_count, 2);
        assert_eq!(a.requests, 2);
        assert!(close(a.cost_usd, 0.03));
        assert!(close(a.saved_usd, 0.0));
        assert_eq!((a.tokens_in, a.tokens_out), (30, 15));
        assert_eq!(a.estimated_rows, 0);
        assert_eq!((a.first_at.as_str(), a.last_at.as_str()), (rows[1].created_at.as_str(), rows[2].created_at.as_str()));

        let b = &runs[0];
        assert_eq!(b.variant, "candidate");
        assert!(!b.mixed());
        assert_eq!(b.requests, 1, "the unbound turn is not a stamped request");
        assert!(close(b.cost_usd, 0.04));
        assert_eq!(b.estimated_rows, 1);
        assert_eq!(b.last_at, rows[3].created_at, "the unbound turn does not extend the span");

        let page = db.experiment_runs(7, 1, 1).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].correlation_id, "a");
        assert!(db.experiment_runs(7, 10, 3).await.unwrap().is_empty());
        assert!(db.experiment_runs(9, 10, 0).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn experiment_run_keys_cover_every_run() {
    each_backend(|db| async move {
        let (alice, bob, rows) = seed_experiment(&db).await;
        let keys = db.experiment_run_keys(7).await.unwrap();
        let summary: Vec<(i64, &str, &str, bool, i64)> = keys
            .iter()
            .map(|k| (k.user_id, k.correlation_id.as_str(), k.variant.as_str(), k.mixed, k.requests))
            .collect();
        let mut expected = vec![
            (alice, "a", "control", true, 2),
            (alice, "b", "candidate", false, 1),
            (bob, "b", "control", false, 1),
        ];
        expected.sort();
        assert_eq!(summary, expected);
        let a = keys.iter().find(|k| k.correlation_id == "a").unwrap();
        assert_eq!((a.first_at.as_str(), a.last_at.as_str()), (rows[1].created_at.as_str(), rows[2].created_at.as_str()));
        assert!(db.experiment_run_keys(9).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn experiment_unbound_requests_share_a_bound_runs_key() {
    each_backend(|db| async move {
        let (alice, _, _) = seed_experiment(&db).await;
        let unbound = db.experiment_unbound_requests(7).await.unwrap();
        let rows: Vec<(i64, &str, i64)> =
            unbound.iter().map(|u| (u.user_id, u.correlation_id.as_str(), u.requests)).collect();
        assert_eq!(rows, [(alice, "b", 1)], "run `none` has no bound row and is not counted");
        assert!(db.experiment_unbound_requests(8).await.unwrap().is_empty());
    })
    .await;
}
