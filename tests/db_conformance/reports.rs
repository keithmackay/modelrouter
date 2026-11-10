//! Conformance scenarios: the SQL reports in `modelrouter::report`, which
//! aggregate in the database rather than through a repository trait.

use std::sync::Arc;

use modelrouter::api::app::DatabaseProvider;
use modelrouter::db::models::{
    HookMetric, NewApiKey, NewAuditLogEntry, NewCostLedgerEntry, NewPrompt, NewUser,
};
use modelrouter::db::repositories::audit::AuditRepository;
use modelrouter::db::repositories::costs::CostRepository;
use modelrouter::db::repositories::prompts::PromptRepository;
use modelrouter::db::repositories::UserRepository;
use modelrouter::report::{self, UsageGranularity, UsageScope, UsageWindow};

use crate::harness::each_backend_with_reports;

type Db = Arc<dyn DatabaseProvider>;

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

async fn user(db: &Db, name: &str) -> i64 {
    UserRepository::create(&**db, NewUser { name: name.into(), email: None })
        .await
        .unwrap()
        .id
}

fn ledger(user_id: i64, model: &str, project: Option<&str>, cost_usd: f64) -> NewCostLedgerEntry {
    NewCostLedgerEntry {
        user_id,
        prompt_id: None,
        model: model.into(),
        provider: "p".into(),
        project: project.map(str::to_string),
        tokens_in: 100,
        tokens_out: 40,
        cost_usd,
        api_key_id: None,
        attribution_correlation_id: None,
        attribution_tags: "{}".into(),
        experiment_id: None,
        experiment_variant: None,
        tokens_estimated: false,
    }
}

/// alice: two `m1` rows in project `a` (one through a labelled key) and an
/// `m2` row with no project; bob: one `m1` row in project `b`. alice is in
/// group `team`.
async fn seed_ledger(db: &Db) -> (i64, i64, i64) {
    let alice = user(db, "alice").await;
    let bob = user(db, "bob").await;
    let key = db
        .create_api_key(NewApiKey {
            user_id: alice,
            key_hash: "h-alice".into(),
            label: Some("laptop".into()),
            expires_at: None,
            project: Some("a".into()),
            session_window_secs: None,
        })
        .await
        .unwrap()
        .id;
    let group = db.create_group("team", 0).await.unwrap();
    db.add_member(group.id, alice).await.unwrap();

    CostRepository::create(&**db, NewCostLedgerEntry { api_key_id: Some(key), ..ledger(alice, "m1", Some("a"), 1.0) })
        .await
        .unwrap();
    CostRepository::create(&**db, ledger(alice, "m1", Some("a"), 2.0)).await.unwrap();
    CostRepository::create(&**db, ledger(alice, "m2", None, 4.0)).await.unwrap();
    CostRepository::create(&**db, ledger(bob, "m1", Some("b"), 8.0)).await.unwrap();
    (alice, bob, key)
}

#[tokio::test]
async fn cost_by_user_window_groups_and_filters() {
    each_backend_with_reports(|db, pool| async move {
        let (_, _, key) = seed_ledger(&db).await;

        let rows = report::cost_by_user_window(&pool, "alltime", None, None, None, None, None)
            .await
            .unwrap();
        // One row per (user, model, project, key), most expensive first.
        let got: Vec<(&str, &str, &str, f64, i64)> = rows
            .iter()
            .map(|r| (r.user_name.as_str(), r.model.as_str(), r.project.as_str(), r.total_cost_usd, r.request_count))
            .collect();
        assert_eq!(
            got,
            vec![
                ("bob", "m1", "b", 8.0, 1),
                ("alice", "m2", "", 4.0, 1),
                ("alice", "m1", "a", 2.0, 1),
                ("alice", "m1", "a", 1.0, 1),
            ]
        );
        let keyed = rows.iter().find(|r| close(r.total_cost_usd, 1.0)).unwrap();
        assert_eq!(keyed.key, "a (laptop)");
        assert_eq!(keyed.groups, "team");
        assert_eq!((keyed.total_tokens_in, keyed.total_tokens_out), (100, 40));
        assert_eq!(rows[0].groups, "");

        let by_user = report::cost_by_user_window(&pool, "monthly", Some("alice"), None, None, None, None)
            .await
            .unwrap();
        assert_eq!(by_user.len(), 3);
        let by_project = report::cost_by_user_window(&pool, "alltime", None, Some("a"), None, None, None)
            .await
            .unwrap();
        assert_eq!(by_project.len(), 2);
        let by_group = report::cost_by_user_window(&pool, "alltime", None, None, Some("team"), None, None)
            .await
            .unwrap();
        assert!(by_group.iter().all(|r| r.user_name == "alice"));
        let by_model = report::cost_by_user_window(&pool, "alltime", None, None, None, Some("m2"), None)
            .await
            .unwrap();
        assert_eq!(by_model.len(), 1);
        let by_key = report::cost_by_user_window(&pool, "alltime", None, None, None, None, Some(key))
            .await
            .unwrap();
        assert_eq!(by_key.len(), 1);
        let no_group = report::cost_by_user_window(&pool, "alltime", None, None, Some("nobody"), None, None)
            .await
            .unwrap();
        assert!(no_group.is_empty());
    })
    .await;
}

#[tokio::test]
async fn usage_by_model_sums_per_model_and_provider() {
    each_backend_with_reports(|db, pool| async move {
        seed_ledger(&db).await;
        let rows = report::usage_by_model(&pool, None, None, None).await.unwrap();
        let got: Vec<(&str, i64, i64, f64)> = rows
            .iter()
            .map(|r| (r.model.as_str(), r.request_count, r.total_tokens_in, r.total_cost_usd))
            .collect();
        assert_eq!(got, vec![("m1", 3, 300, 11.0), ("m2", 1, 100, 4.0)]);

        let proj = report::usage_by_model(&pool, None, Some("m1"), Some("a")).await.unwrap();
        assert_eq!(proj.len(), 1);
        assert_eq!(proj[0].request_count, 2);
    })
    .await;
}

#[tokio::test]
async fn usage_rows_total_subtotal_and_detail() {
    each_backend_with_reports(|db, pool| async move {
        seed_ledger(&db).await;
        let total = report::fetch_usage_rows(&pool, &UsageScope::Global, &UsageWindow::AllTime, &UsageGranularity::Total)
            .await
            .unwrap();
        assert_eq!(total.len(), 1);
        assert_eq!((total[0].tokens_in, total[0].tokens_out), (400, 160));
        assert!(close(total[0].cost_usd, 15.0));
        assert_eq!(total[0].bucket, None);

        let monthly = report::fetch_usage_rows(&pool, &UsageScope::Global, &UsageWindow::Monthly, &UsageGranularity::Total)
            .await
            .unwrap();
        let this_month = chrono::Utc::now().format("%Y-%m").to_string();
        assert_eq!(monthly.len(), 1);
        assert_eq!(monthly[0].bucket.as_deref(), Some(this_month.as_str()));

        let annual = report::fetch_usage_rows(&pool, &UsageScope::User("bob".into()), &UsageWindow::Annual, &UsageGranularity::Subtotal)
            .await
            .unwrap();
        assert_eq!(annual.len(), 1);
        assert_eq!(annual[0].bucket.as_deref(), Some(&this_month[..4]));
        assert_eq!(annual[0].user_name.as_deref(), Some("bob"));
        assert!(close(annual[0].cost_usd, 8.0));

        let subtotal = report::fetch_usage_rows(&pool, &UsageScope::Group("team".into()), &UsageWindow::AllTime, &UsageGranularity::Subtotal)
            .await
            .unwrap();
        // alice's rows split by project: none, then "a".
        let got: Vec<(Option<&str>, f64)> = subtotal.iter().map(|r| (r.project.as_deref(), r.cost_usd)).collect();
        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|(p, c)| *p == Some("a") && close(*c, 3.0)));
        assert!(got.iter().any(|(p, c)| p.is_none() && close(*c, 4.0)));

        let detail = report::fetch_usage_rows(
            &pool,
            &UsageScope::UserInProject { user: "alice".into(), project: "a".into() },
            &UsageWindow::AllTime,
            &UsageGranularity::Detail,
        )
        .await
        .unwrap();
        assert_eq!(detail.len(), 2);
        assert!(detail.iter().all(|r| r.model.as_deref() == Some("m1")));

        let in_group = report::fetch_usage_rows(
            &pool,
            &UsageScope::UserInGroup { user: "bob".into(), group: "team".into() },
            &UsageWindow::AllTime,
            &UsageGranularity::Total,
        )
        .await
        .unwrap();
        // bob is not in the group: an all-time total over no rows.
        assert_eq!(in_group.len(), 1);
        assert_eq!(in_group[0].tokens_in, 0);

        let by_project = report::fetch_usage_rows(&pool, &UsageScope::Project("b".into()), &UsageWindow::AllTime, &UsageGranularity::Total)
            .await
            .unwrap();
        assert!(close(by_project[0].cost_usd, 8.0));
    })
    .await;
}

#[tokio::test]
async fn recent_prompts_newest_first_with_filters() {
    each_backend_with_reports(|db, pool| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        let prompt = |user_id: i64, model: &str, cache_read_tokens: i64| NewPrompt {
            user_id,
            session_id: None,
            request_model: model.into(),
            routed_model: model.into(),
            provider: "p".into(),
            messages: "[]".into(),
            response: None,
            finish_reason: None,
            prompt_tokens: 10,
            completion_tokens: 5,
            cache_read_tokens,
            cache_write_tokens: 0,
            cost_usd: 0.5,
            latency_ms: None,
            ttft_ms: None,
            attempts: None,
            tags: "[]".into(),
            project: None,
            attribution_correlation_id: None,
            attribution_tags: "{}".into(),
            experiment_id: None,
            experiment_variant: None,
        };
        PromptRepository::create(&*db, prompt(alice, "first", 0)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        PromptRepository::create(&*db, prompt(bob, "second", 7)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        PromptRepository::create(&*db, prompt(alice, "third", 0)).await.unwrap();

        let all = report::recent_prompts(&pool, None, 10, None).await.unwrap();
        let models: Vec<&str> = all.iter().map(|r| r.request_model.as_str()).collect();
        assert_eq!(models, vec!["third", "second", "first"]);
        assert!(all[1].cached && !all[0].cached);

        let limited = report::recent_prompts(&pool, None, 1, None).await.unwrap();
        assert_eq!(limited.len(), 1);
        let alices = report::recent_prompts(&pool, Some("alice"), 10, None).await.unwrap();
        assert_eq!(alices.len(), 2);
        let future = report::recent_prompts(&pool, None, 10, Some("9999-01-01T00:00:00Z")).await.unwrap();
        assert!(future.is_empty());
    })
    .await;
}

#[tokio::test]
async fn recent_audit_newest_first_by_actor() {
    each_backend_with_reports(|db, pool| async move {
        for (actor, action) in [("cli", "one"), ("ops", "two"), ("cli", "three")] {
            AuditRepository::create(
                &*db,
                NewAuditLogEntry {
                    actor_id: None,
                    actor_name: actor.into(),
                    action: action.into(),
                    target: Some("t".into()),
                    before_json: None,
                    after_json: None,
                },
            )
            .await
            .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        }
        let all = report::recent_audit(&pool, None, 10).await.unwrap();
        let actions: Vec<&str> = all.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, vec!["three", "two", "one"]);
        let cli = report::recent_audit(&pool, Some("cli"), 1).await.unwrap();
        assert_eq!(cli.len(), 1);
        assert_eq!(cli[0].action, "three");
    })
    .await;
}

#[tokio::test]
async fn hook_latency_stats_reads_boolean_success() {
    each_backend_with_reports(|db, pool| async move {
        for (ms, ok) in [(10, true), (20, false), (30, true), (40, true)] {
            db.record_metric(HookMetric {
                hook_name: "h".into(),
                invoked_at: chrono::Utc::now().to_rfc3339(),
                duration_ms: ms,
                success: ok,
            })
            .await
            .unwrap();
        }
        let stats = report::hook_latency_stats(&pool).await.unwrap();
        assert_eq!(stats.len(), 1);
        let h = &stats[0];
        assert_eq!(h.invocation_count, 4);
        assert!(close(h.success_rate, 0.75));
        assert!(close(h.avg_duration_ms, 25.0));
        assert_eq!((h.p50_duration_ms, h.p99_duration_ms), (20, 40));
    })
    .await;
}
