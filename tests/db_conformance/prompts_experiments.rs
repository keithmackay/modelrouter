//! Conformance scenarios: prompts_experiments repositories.

#[allow(unused_imports)]
use crate::harness::each_backend;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use modelrouter::api::app::DatabaseProvider;
use modelrouter::db::models::{
    ExperimentStatus, ExperimentVariants, FailureStage, NewApiKey, NewExperiment, NewPrompt,
    NewRequestFailure, NewRunOutcome, NewUser, VariantTarget,
};
use modelrouter::db::prompt_store::CONTENT_NOT_STORED;
use modelrouter::db::repositories::costs::{ArmFilter, AttributionFilter};
use modelrouter::db::repositories::experiments::ExperimentStatusFilter;
use modelrouter::db::repositories::prompts::{AttemptsSummary, LatencySummary};
use modelrouter::db::repositories::{
    ExperimentRepository, FailureRepository, OutcomeRepository,
    PromptRepository, UserRepository,
};

/// A window that contains every row a scenario writes: the repositories stamp
/// `created_at` with the current time.
const W_START: &str = "2000-01-01T00:00:00+00:00";
const W_END: &str = "2100-01-01T00:00:00+00:00";

async fn user(db: &Arc<dyn DatabaseProvider>, name: &str) -> i64 {
    UserRepository::create(&**db, NewUser { name: name.into(), email: None })
        .await
        .unwrap()
        .id
}

/// Rows ordered by `created_at` need distinct timestamps.
async fn tick() {
    tokio::time::sleep(Duration::from_millis(3)).await;
}

/// The current time in the repositories' `created_at` format, strictly
/// between the rows written before and after it.
async fn cutoff() -> String {
    tick().await;
    let now = chrono::Utc::now().to_rfc3339();
    tick().await;
    now
}

fn prompt(user_id: i64) -> NewPrompt {
    NewPrompt {
        user_id,
        session_id: None,
        request_model: "req".into(),
        routed_model: "X".into(),
        provider: "p".into(),
        messages: "[]".into(),
        response: None,
        finish_reason: None,
        prompt_tokens: 0,
        completion_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost_usd: 0.0,
        latency_ms: None,
        ttft_ms: None,
        attempts: None,
        tags: "[]".into(),
        project: None,
        attribution_correlation_id: None,
        attribution_tags: "{}".into(),
        experiment_id: None,
        experiment_variant: None,
    }
}

async fn add_prompt(db: &Arc<dyn DatabaseProvider>, p: NewPrompt) -> i64 {
    PromptRepository::create(&**db, p).await.unwrap().id
}

fn latency_row(user_id: i64, routed_model: &str, latency_ms: Option<i64>) -> NewPrompt {
    NewPrompt { routed_model: routed_model.into(), latency_ms, ..prompt(user_id) }
}

fn model(m: &str) -> ArmFilter {
    ArmFilter::Model(m.into())
}

fn failure(request_model: &str) -> NewRequestFailure {
    NewRequestFailure {
        user_id: None,
        api_key_id: None,
        endpoint: "/v1/chat/completions".into(),
        request_model: request_model.into(),
        routed_model: None,
        provider: None,
        stage: FailureStage::Provider,
        status_code: None,
        error_message: "boom".into(),
        attempts: 1,
        latency_ms: None,
        project: None,
        attribution_correlation_id: None,
        attribution_tags: "{}".into(),
        experiment_id: None,
        experiment_variant: None,
    }
}

async fn add_failure(db: &Arc<dyn DatabaseProvider>, f: NewRequestFailure) -> i64 {
    FailureRepository::create(&**db, f).await.unwrap().id
}

fn target(expr: &str, provider: &str, model: &str) -> VariantTarget {
    VariantTarget { target: expr.into(), provider: provider.into(), model: model.into() }
}

fn variants() -> ExperimentVariants {
    let mut control = BTreeMap::new();
    control.insert("fast".to_string(), target("fast", "openai", "gpt-5-mini"));
    let mut candidate = BTreeMap::new();
    candidate.insert("fast".to_string(), target("anthropic/claude-haiku", "anthropic", "claude-haiku"));
    candidate.insert("deep".to_string(), target("deep", "anthropic", "claude-opus"));
    let mut v = BTreeMap::new();
    v.insert("control".to_string(), control);
    v.insert("candidate".to_string(), candidate);
    v
}

fn new_experiment(name: &str, expires_at: i64) -> NewExperiment {
    NewExperiment {
        name: name.to_string(),
        variants: variants(),
        allowed_user_ids: vec![3, 7],
        feed_learning: true,
        expires_at,
        retain_content: true,
        content_retention_days: 0,
    }
}

fn outcome(user_id: i64, run: &str, outcome: &str, experiment_id: Option<i64>) -> NewRunOutcome {
    NewRunOutcome {
        user_id,
        attribution_correlation_id: run.to_string(),
        outcome: outcome.to_string(),
        score: Some(0.5),
        rating: Some(3),
        note: Some("first pass".to_string()),
        experiment_id,
        experiment_variant: experiment_id.map(|_| "control".to_string()),
    }
}

// ---- prompts ----

#[tokio::test]
async fn prompt_create_round_trips_every_column() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        let new = NewPrompt {
            user_id: uid,
            session_id: None,
            request_model: "fast".into(),
            routed_model: "gpt-5-mini".into(),
            provider: "openai".into(),
            messages: r#"[{"role":"user","content":"hi"}]"#.into(),
            response: Some("hello".into()),
            finish_reason: Some("stop".into()),
            prompt_tokens: 12,
            completion_tokens: 34,
            cache_read_tokens: 5,
            cache_write_tokens: 6,
            cost_usd: 0.012_5,
            latency_ms: Some(420),
            ttft_ms: Some(90),
            attempts: Some(2),
            tags: r#"["a","b"]"#.into(),
            project: Some("proj".into()),
            attribution_correlation_id: Some("run-1".into()),
            attribution_tags: r#"{"arm":"a"}"#.into(),
            experiment_id: Some(4),
            experiment_variant: Some("control".into()),
        };
        let saved = PromptRepository::create(&*db, new).await.unwrap();
        assert!(saved.id > 0);
        assert_eq!(saved.user_id, uid);
        assert_eq!(saved.session_id, None);
        assert_eq!(saved.request_model, "fast");
        assert_eq!(saved.routed_model, "gpt-5-mini");
        assert_eq!(saved.provider, "openai");
        assert_eq!(saved.messages, r#"[{"role":"user","content":"hi"}]"#);
        assert_eq!(saved.response.as_deref(), Some("hello"));
        assert_eq!(saved.finish_reason.as_deref(), Some("stop"));
        assert_eq!(saved.prompt_tokens, 12);
        assert_eq!(saved.completion_tokens, 34);
        assert_eq!(saved.cache_read_tokens, 5);
        assert_eq!(saved.cache_write_tokens, 6);
        assert!(saved.is_cached());
        assert_eq!(saved.cost_usd, 0.012_5);
        assert_eq!(saved.latency_ms, Some(420));
        assert_eq!(saved.ttft_ms, Some(90));
        assert_eq!(saved.attempts, Some(2));
        assert_eq!(saved.tags, r#"["a","b"]"#);
        assert_eq!(saved.project.as_deref(), Some("proj"));
        assert_eq!(saved.attribution_correlation_id.as_deref(), Some("run-1"));
        assert_eq!(saved.attribution_tags, r#"{"arm":"a"}"#);
        assert_eq!(saved.experiment_id, Some(4));
        assert_eq!(saved.experiment_variant.as_deref(), Some("control"));
        assert!(chrono::DateTime::parse_from_rfc3339(&saved.created_at).is_ok());

        let got = PromptRepository::find_by_id(&*db, saved.id).await.unwrap().expect("row exists");
        assert_eq!(serde_json::to_value(&got).unwrap(), serde_json::to_value(&saved).unwrap());
        assert!(PromptRepository::find_by_id(&*db, saved.id + 100).await.unwrap().is_none());

        // Defaults: nothing optional set reads back as None / zero.
        let bare = PromptRepository::create(&*db, prompt(uid)).await.unwrap();
        assert!(!bare.is_cached());
        assert_eq!(bare.response, None);
        assert_eq!(bare.latency_ms, None);
        assert_eq!(bare.ttft_ms, None);
        assert_eq!(bare.attempts, None);
        assert_eq!(bare.attribution_correlation_id, None);
        assert_eq!(bare.experiment_id, None);
        assert_eq!(bare.experiment_variant, None);
    })
    .await;
}

#[tokio::test]
async fn prompt_listing_and_count() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        assert_eq!(PromptRepository::count(&*db).await.unwrap(), 0);
        assert!(PromptRepository::list(&*db, 10, 0).await.unwrap().is_empty());

        let a1 = add_prompt(&db, prompt(alice)).await;
        tick().await;
        let b1 = add_prompt(&db, prompt(bob)).await;
        tick().await;
        let a2 = add_prompt(&db, prompt(alice)).await;
        tick().await;
        let a3 = add_prompt(&db, prompt(alice)).await;

        assert_eq!(PromptRepository::count(&*db).await.unwrap(), 4);

        let ids = |v: Vec<modelrouter::db::models::Prompt>| v.into_iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(ids(db.list_by_user(alice, 10).await.unwrap()), vec![a3, a2, a1]);
        assert_eq!(ids(db.list_by_user(alice, 2).await.unwrap()), vec![a3, a2]);
        assert_eq!(ids(db.list_by_user(bob, 10).await.unwrap()), vec![b1]);
        assert!(db.list_by_user(bob + 100, 10).await.unwrap().is_empty());

        assert_eq!(ids(PromptRepository::list(&*db, 10, 0).await.unwrap()), vec![a3, a2, b1, a1]);
        assert_eq!(ids(PromptRepository::list(&*db, 2, 1).await.unwrap()), vec![a2, b1]);
        assert!(PromptRepository::list(&*db, 10, 4).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn prompt_purge_older_than() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        let old1 = add_prompt(&db, prompt(uid)).await;
        let old2 = add_prompt(&db, NewPrompt { experiment_id: Some(7), ..prompt(uid) }).await;
        let cut = cutoff().await;
        let new1 = add_prompt(&db, prompt(uid)).await;

        assert_eq!(db.purge_older_than(W_START).await.unwrap(), 0);
        assert_eq!(db.purge_older_than(&cut).await.unwrap(), 2);
        assert!(PromptRepository::find_by_id(&*db, old1).await.unwrap().is_none());
        assert!(PromptRepository::find_by_id(&*db, old2).await.unwrap().is_none());
        assert!(PromptRepository::find_by_id(&*db, new1).await.unwrap().is_some());
        assert_eq!(db.purge_older_than(&cut).await.unwrap(), 0);
    })
    .await;
}

#[tokio::test]
async fn prompt_purge_older_than_except_spares_the_listed_experiments() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        let stamped = |exp: Option<i64>| NewPrompt { experiment_id: exp, ..prompt(uid) };
        let old_plain = add_prompt(&db, stamped(None)).await;
        let old_kept = add_prompt(&db, stamped(Some(7))).await;
        let old_kept_too = add_prompt(&db, stamped(Some(8))).await;
        let old_gone = add_prompt(&db, stamped(Some(9))).await;
        let cut = cutoff().await;
        let new_plain = add_prompt(&db, stamped(None)).await;
        let new_gone_exp = add_prompt(&db, stamped(Some(9))).await;

        let deleted = db.purge_older_than_except(&cut, &[7, 8]).await.unwrap();
        assert_eq!(deleted, 2);
        for (id, present) in [
            (old_plain, false),
            (old_kept, true),
            (old_kept_too, true),
            (old_gone, false),
            (new_plain, true),
            (new_gone_exp, true),
        ] {
            assert_eq!(
                PromptRepository::find_by_id(&*db, id).await.unwrap().is_some(),
                present,
                "row {id}"
            );
        }

        // No exceptions: a plain purge, stamped rows included.
        assert_eq!(db.purge_older_than_except(&cut, &[]).await.unwrap(), 2);
        assert_eq!(PromptRepository::count(&*db).await.unwrap(), 2);
    })
    .await;
}

#[tokio::test]
async fn prompt_purge_older_than_except_handles_more_experiments_than_one_chunk() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        // More distinct expendable experiments than one DELETE binds (500).
        let mut rows = Vec::new();
        for exp in 1..=520_i64 {
            rows.push((exp, add_prompt(&db, NewPrompt { experiment_id: Some(exp), ..prompt(uid) }).await));
        }
        let cut = cutoff().await;
        let kept: Vec<i64> = vec![3, 505];
        assert_eq!(db.purge_older_than_except(&cut, &kept).await.unwrap(), 518);
        assert_eq!(PromptRepository::count(&*db).await.unwrap(), 2);
        for (exp, id) in rows {
            assert_eq!(
                PromptRepository::find_by_id(&*db, id).await.unwrap().is_some(),
                kept.contains(&exp),
                "experiment {exp}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn prompt_redact_experiment_content() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        let content = |exp: Option<i64>, response: Option<&str>| NewPrompt {
            messages: "secret prompt".into(),
            response: response.map(str::to_string),
            prompt_tokens: 10,
            latency_ms: Some(100),
            experiment_id: exp,
            experiment_variant: exp.map(|_| "control".to_string()),
            ..prompt(uid)
        };
        let a = add_prompt(&db, content(Some(7), Some("secret answer"))).await;
        let b = add_prompt(&db, content(Some(7), None)).await;
        // Already redacted: nothing left to touch.
        let c = add_prompt(&db, NewPrompt { messages: CONTENT_NOT_STORED.into(), ..content(Some(7), None) }).await;
        let other = add_prompt(&db, content(Some(8), Some("keep"))).await;
        let plain = add_prompt(&db, content(None, Some("keep"))).await;

        assert_eq!(db.redact_experiment_content(7).await.unwrap(), 2);
        for id in [a, b, c] {
            let p = PromptRepository::find_by_id(&*db, id).await.unwrap().unwrap();
            assert_eq!(p.messages, CONTENT_NOT_STORED);
            assert_eq!(p.response, None);
            // Everything the results page reads survives.
            assert_eq!(p.prompt_tokens, 10);
            assert_eq!(p.latency_ms, Some(100));
            assert_eq!(p.experiment_id, Some(7));
            assert_eq!(p.experiment_variant.as_deref(), Some("control"));
        }
        for id in [other, plain] {
            let p = PromptRepository::find_by_id(&*db, id).await.unwrap().unwrap();
            assert_eq!(p.messages, "secret prompt");
            assert_eq!(p.response.as_deref(), Some("keep"));
        }
        // Idempotent.
        assert_eq!(db.redact_experiment_content(7).await.unwrap(), 0);
        assert_eq!(db.redact_experiment_content(99).await.unwrap(), 0);
    })
    .await;
}

#[tokio::test]
async fn prompt_latency_summary_mean_and_nearest_rank_percentiles() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        for ms in [100, 200, 300, 400, 500, 600, 700, 800, 900, 1000] {
            add_prompt(&db, latency_row(uid, "X", Some(ms))).await;
        }
        // Cache hits and unmeasured rows are not samples.
        add_prompt(&db, latency_row(uid, "X", Some(0))).await;
        add_prompt(&db, latency_row(uid, "X", None)).await;
        add_prompt(&db, latency_row(uid, "Y", Some(7000))).await;

        let s = db.latency_summary(&model("X"), W_START, W_END).await.unwrap();
        assert_eq!(
            s,
            LatencySummary { samples: 10, mean_ms: Some(550.0), p50_ms: Some(500), p95_ms: Some(1000) }
        );

        let one = db.latency_summary(&model("Y"), W_START, W_END).await.unwrap();
        assert_eq!(
            one,
            LatencySummary { samples: 1, mean_ms: Some(7000.0), p50_ms: Some(7000), p95_ms: Some(7000) }
        );

        // Empty arm, and a window that excludes everything.
        assert_eq!(db.latency_summary(&model("Z"), W_START, W_END).await.unwrap(), LatencySummary::default());
        assert_eq!(
            db.latency_summary(&model("X"), W_START, "2001-01-01T00:00:00+00:00").await.unwrap(),
            LatencySummary::default()
        );
    })
    .await;
}

#[tokio::test]
async fn prompt_latency_summary_window_is_half_open() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        add_prompt(&db, latency_row(uid, "X", Some(100))).await;
        let cut = cutoff().await;
        add_prompt(&db, latency_row(uid, "X", Some(300))).await;

        let before = db.latency_summary(&model("X"), W_START, &cut).await.unwrap();
        assert_eq!(before.samples, 1);
        assert_eq!(before.p50_ms, Some(100));
        let after = db.latency_summary(&model("X"), &cut, W_END).await.unwrap();
        assert_eq!(after.samples, 1);
        assert_eq!(after.p50_ms, Some(300));
    })
    .await;
}

#[tokio::test]
async fn prompt_latency_summary_provider_tag_run_and_variant_arms() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        add_prompt(&db, NewPrompt {
            provider: "p1".into(),
            attribution_correlation_id: Some("run-1".into()),
            attribution_tags: r#"{"arm":"a"}"#.into(),
            experiment_id: Some(7),
            experiment_variant: Some("control".into()),
            ..latency_row(uid, "X", Some(100))
        })
        .await;
        add_prompt(&db, NewPrompt {
            provider: "p1".into(),
            attribution_correlation_id: Some("run-10".into()),
            attribution_tags: r#"{"arm":"b"}"#.into(),
            experiment_id: Some(7),
            experiment_variant: Some("candidate".into()),
            ..latency_row(uid, "Y", Some(300))
        })
        .await;
        add_prompt(&db, NewPrompt { provider: "p2".into(), ..latency_row(uid, "X", Some(900)) }).await;

        let p1 = db.latency_summary(&ArmFilter::Provider("p1".into()), W_START, W_END).await.unwrap();
        assert_eq!(p1.samples, 2);
        assert_eq!(p1.mean_ms, Some(200.0));

        let tag = ArmFilter::Attribution(AttributionFilter::Tag { key: "arm".into(), value: "a".into() });
        let t = db.latency_summary(&tag, W_START, W_END).await.unwrap();
        assert_eq!((t.samples, t.p50_ms), (1, Some(100)));

        // Exact match: run-1 does not pick up run-10.
        let run = ArmFilter::Attribution(AttributionFilter::CorrelationId("run-1".into()));
        let r = db.latency_summary(&run, W_START, W_END).await.unwrap();
        assert_eq!((r.samples, r.p50_ms), (1, Some(100)));

        let variant = ArmFilter::Variant { experiment_id: 7, variant: "candidate".into() };
        let v = db.latency_summary(&variant, W_START, W_END).await.unwrap();
        assert_eq!((v.samples, v.p50_ms), (1, Some(300)));
        let none = ArmFilter::Variant { experiment_id: 8, variant: "candidate".into() };
        assert_eq!(db.latency_summary(&none, W_START, W_END).await.unwrap(), LatencySummary::default());
    })
    .await;
}

#[tokio::test]
async fn prompt_ttft_summary_reads_ttft_not_latency() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        for ms in [50, 150, 250, 350] {
            add_prompt(&db, NewPrompt { ttft_ms: Some(ms), latency_ms: Some(9999), ..prompt(uid) }).await;
        }
        add_prompt(&db, NewPrompt { ttft_ms: Some(0), ..prompt(uid) }).await;
        add_prompt(&db, NewPrompt { ttft_ms: None, latency_ms: Some(300), ..prompt(uid) }).await;

        let s = db.ttft_summary(&model("X"), W_START, W_END).await.unwrap();
        assert_eq!(
            s,
            LatencySummary { samples: 4, mean_ms: Some(200.0), p50_ms: Some(150), p95_ms: Some(350) }
        );
        assert_eq!(db.ttft_summary(&model("Y"), W_START, W_END).await.unwrap(), LatencySummary::default());
    })
    .await;
}

#[tokio::test]
async fn prompt_attempts_summary_totals_and_retried_counts() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        let row = |m: &str, attempts: Option<i64>| NewPrompt { routed_model: m.into(), attempts, ..prompt(uid) };
        for n in [1, 1, 2, 3] {
            add_prompt(&db, row("X", Some(n))).await;
        }
        add_prompt(&db, row("X", None)).await;
        add_prompt(&db, row("Y", Some(9))).await;

        assert_eq!(
            db.attempts_summary(&model("X"), W_START, W_END).await.unwrap(),
            AttemptsSummary { requests_tracked: 4, attempts: 7, retried_requests: 2 }
        );
        assert_eq!(
            db.attempts_summary(&model("Y"), W_START, W_END).await.unwrap(),
            AttemptsSummary { requests_tracked: 1, attempts: 9, retried_requests: 1 }
        );
        assert_eq!(
            db.attempts_summary(&model("Z"), W_START, W_END).await.unwrap(),
            AttemptsSummary::default()
        );
    })
    .await;
}

#[tokio::test]
async fn prompt_experiment_run_latency_groups_by_user_and_run() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        let row = |uid: i64, run: Option<&str>, exp: i64, ms: Option<i64>| NewPrompt {
            attribution_correlation_id: run.map(str::to_string),
            experiment_id: Some(exp),
            experiment_variant: Some("control".into()),
            latency_ms: ms,
            ..prompt(uid)
        };
        add_prompt(&db, row(alice, Some("b"), 7, Some(100))).await;
        add_prompt(&db, row(alice, Some("b"), 7, Some(300))).await;
        add_prompt(&db, row(alice, Some("b"), 7, Some(0))).await;
        add_prompt(&db, row(alice, Some("a"), 7, Some(50))).await;
        add_prompt(&db, row(bob, Some("a"), 7, Some(80))).await;
        // Byte order: "B" sorts before "a" whatever the database locale.
        add_prompt(&db, row(bob, Some("B"), 7, Some(60))).await;
        // Not samples, no run, or another experiment.
        add_prompt(&db, row(bob, Some("z"), 7, None)).await;
        add_prompt(&db, row(alice, None, 7, Some(999))).await;
        add_prompt(&db, row(alice, Some("a"), 8, Some(999))).await;

        let runs = db.experiment_run_latency(7).await.unwrap();
        let got: Vec<(i64, String, i64, Option<f64>)> = runs
            .into_iter()
            .map(|r| (r.user_id, r.correlation_id, r.samples, r.mean_ms))
            .collect();
        let mut expected = vec![
            (alice, "a".to_string(), 1, Some(50.0)),
            (alice, "b".to_string(), 2, Some(200.0)),
            (bob, "a".to_string(), 1, Some(80.0)),
            (bob, "B".to_string(), 1, Some(60.0)),
        ];
        expected.sort_by(|x, y| (x.0, &x.1).cmp(&(y.0, &y.1)));
        assert_eq!(got, expected);
        assert!(db.experiment_run_latency(99).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn prompt_experiment_content_bytes_counts_bytes_not_chars() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        assert_eq!(db.experiment_content_bytes(7).await.unwrap(), 0);
        let row = |exp: Option<i64>, messages: &str, response: Option<&str>| NewPrompt {
            messages: messages.into(),
            response: response.map(str::to_string),
            experiment_id: exp,
            ..prompt(uid)
        };
        // "héllo" is 6 bytes, "日本" is 6 bytes.
        add_prompt(&db, row(Some(7), "héllo", Some("日本"))).await;
        add_prompt(&db, row(Some(7), "abcd", None)).await;
        add_prompt(&db, row(Some(8), "ignored", Some("ignored"))).await;
        add_prompt(&db, row(None, "ignored", None)).await;

        assert_eq!(db.experiment_content_bytes(7).await.unwrap(), 6 + 6 + 4);
        assert_eq!(db.experiment_content_bytes(8).await.unwrap(), 14);
    })
    .await;
}

// ---- failures ----

#[tokio::test]
async fn failure_create_round_trips_every_column() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        let key = db
            .create_api_key(NewApiKey {
                user_id: uid,
                key_hash: "f".repeat(64),
                label: None,
                expires_at: None,
                project: None,
                session_window_secs: None,
            })
            .await
            .unwrap();
        let saved = FailureRepository::create(&*db, NewRequestFailure {
            user_id: Some(uid),
            api_key_id: Some(key.id),
            endpoint: "/v1/chat/completions".into(),
            request_model: "fast".into(),
            routed_model: Some("gpt-5-mini".into()),
            provider: Some("openai".into()),
            stage: FailureStage::Provider,
            status_code: Some(503),
            error_message: "upstream unavailable".into(),
            attempts: 3,
            latency_ms: Some(1234),
            project: Some("proj".into()),
            attribution_correlation_id: Some("run-1".into()),
            attribution_tags: r#"{"arm":"a"}"#.into(),
            experiment_id: Some(4),
            experiment_variant: Some("control".into()),
        })
        .await
        .unwrap();
        assert!(saved.id > 0);
        assert_eq!(saved.user_id, Some(uid));
        assert_eq!(saved.api_key_id, Some(key.id));
        assert_eq!(saved.endpoint, "/v1/chat/completions");
        assert_eq!(saved.request_model, "fast");
        assert_eq!(saved.routed_model.as_deref(), Some("gpt-5-mini"));
        assert_eq!(saved.provider.as_deref(), Some("openai"));
        assert_eq!(saved.stage, "provider");
        assert_eq!(saved.status_code, Some(503));
        assert_eq!(saved.error_message, "upstream unavailable");
        assert_eq!(saved.attempts, 3);
        assert_eq!(saved.latency_ms, Some(1234));
        assert_eq!(saved.project.as_deref(), Some("proj"));
        assert_eq!(saved.attribution_correlation_id.as_deref(), Some("run-1"));
        assert_eq!(saved.attribution_tags, r#"{"arm":"a"}"#);
        assert_eq!(saved.experiment_id, Some(4));
        assert_eq!(saved.experiment_variant.as_deref(), Some("control"));
        assert!(chrono::DateTime::parse_from_rfc3339(&saved.created_at).is_ok());

        let got = FailureRepository::find_by_id(&*db, saved.id).await.unwrap().expect("row exists");
        assert_eq!(serde_json::to_value(&got).unwrap(), serde_json::to_value(&saved).unwrap());
        assert!(FailureRepository::find_by_id(&*db, saved.id + 100).await.unwrap().is_none());

        // An anonymous failure before routing: every optional column empty.
        let bare = FailureRepository::create(&*db, NewRequestFailure {
            stage: FailureStage::Resolve,
            ..failure("unknown/model")
        })
        .await
        .unwrap();
        assert_eq!(bare.user_id, None);
        assert_eq!(bare.api_key_id, None);
        assert_eq!(bare.routed_model, None);
        assert_eq!(bare.status_code, None);
        assert_eq!(bare.stage, "resolve");
        assert_eq!(bare.experiment_id, None);
    })
    .await;
}

#[tokio::test]
async fn failure_listing_counts_and_stages() {
    each_backend(|db| async move {
        assert_eq!(FailureRepository::count(&*db).await.unwrap(), 0);
        assert!(db.count_by_stage().await.unwrap().is_empty());

        let mut ids = Vec::new();
        for stage in [
            FailureStage::Provider,
            FailureStage::Policy,
            FailureStage::Provider,
            FailureStage::Request,
            FailureStage::Provider,
            FailureStage::Policy,
            FailureStage::Internal,
        ] {
            ids.push(add_failure(&db, NewRequestFailure { stage, ..failure("X") }).await);
            tick().await;
        }
        assert_eq!(FailureRepository::count(&*db).await.unwrap(), 7);

        let mut by_stage = db.count_by_stage().await.unwrap();
        // Highest count first; the two singletons tie, so compare them as a set.
        assert_eq!(&by_stage[..2], &[("provider".to_string(), 3), ("policy".to_string(), 2)]);
        by_stage[2..].sort();
        assert_eq!(&by_stage[2..], &[("internal".to_string(), 1), ("request".to_string(), 1)]);

        let listed: Vec<i64> = FailureRepository::list(&*db, 10, 0).await.unwrap().iter().map(|f| f.id).collect();
        let mut newest_first = ids.clone();
        newest_first.reverse();
        assert_eq!(listed, newest_first);
        let page: Vec<i64> = FailureRepository::list(&*db, 2, 1).await.unwrap().iter().map(|f| f.id).collect();
        assert_eq!(page, newest_first[1..3].to_vec());
        assert!(FailureRepository::list(&*db, 10, 7).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn failure_find_by_correlation_id_is_exact_and_newest_first() {
    each_backend(|db| async move {
        let run = |id: &str| NewRequestFailure { attribution_correlation_id: Some(id.into()), ..failure("X") };
        let a1 = add_failure(&db, run("run-1")).await;
        tick().await;
        add_failure(&db, run("run-10")).await;
        tick().await;
        let a2 = add_failure(&db, run("run-1")).await;
        add_failure(&db, failure("X")).await;

        let got: Vec<i64> = db.find_by_correlation_id("run-1").await.unwrap().iter().map(|f| f.id).collect();
        assert_eq!(got, vec![a2, a1]);
        assert!(db.find_by_correlation_id("run-2").await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn failure_count_for_arm() {
    each_backend(|db| async move {
        let row = |req: &str, routed: Option<&str>, provider: Option<&str>| NewRequestFailure {
            routed_model: routed.map(str::to_string),
            provider: provider.map(str::to_string),
            ..failure(req)
        };
        add_failure(&db, NewRequestFailure {
            attribution_correlation_id: Some("run-1".into()),
            attribution_tags: r#"{"arm":"a"}"#.into(),
            experiment_id: Some(7),
            experiment_variant: Some("control".into()),
            ..row("alias", Some("X"), Some("p1"))
        })
        .await;
        add_failure(&db, NewRequestFailure {
            attribution_correlation_id: Some("run-10".into()),
            attribution_tags: r#"{"arm":"a"}"#.into(),
            experiment_id: Some(7),
            experiment_variant: Some("candidate".into()),
            ..row("X", Some("X"), Some("p1"))
        })
        .await;
        // Failed before routing: matches a model arm by the requested model.
        add_failure(&db, row("X", None, None)).await;
        add_failure(&db, NewRequestFailure {
            attribution_tags: r#"{"arm":"b"}"#.into(),
            ..row("Y", Some("Y"), Some("p2"))
        })
        .await;

        let count = |f: ArmFilter| {
            let db = db.clone();
            async move { db.count_for_arm(&f, W_START, W_END).await.unwrap() }
        };
        assert_eq!(count(model("X")).await, 3);
        assert_eq!(count(model("Y")).await, 1);
        assert_eq!(count(model("alias")).await, 0);
        assert_eq!(count(model("Z")).await, 0);
        assert_eq!(count(ArmFilter::Provider("p1".into())).await, 2);
        let tag = |v: &str| ArmFilter::Attribution(AttributionFilter::Tag { key: "arm".into(), value: v.into() });
        assert_eq!(count(tag("a")).await, 2);
        assert_eq!(count(tag("b")).await, 1);
        assert_eq!(count(ArmFilter::Attribution(AttributionFilter::CorrelationId("run-1".into()))).await, 1);
        assert_eq!(count(ArmFilter::Variant { experiment_id: 7, variant: "control".into() }).await, 1);
        assert_eq!(count(ArmFilter::Variant { experiment_id: 8, variant: "control".into() }).await, 0);

        // Window bounds.
        assert_eq!(db.count_for_arm(&model("X"), W_START, "2001-01-01T00:00:00+00:00").await.unwrap(), 0);
    })
    .await;
}

#[tokio::test]
async fn failure_has_rows_for_user_matches_user_and_correlation_id() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        add_failure(&db, NewRequestFailure {
            user_id: Some(alice),
            attribution_correlation_id: Some("run-1".into()),
            ..failure("X")
        })
        .await;

        assert!(db.has_rows_for_user(alice, "run-1").await.unwrap());
        assert!(!db.has_rows_for_user(bob, "run-1").await.unwrap());
        assert!(!db.has_rows_for_user(alice, "run-2").await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn failure_experiment_run_failures_group_by_run_and_variant() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        let row = |uid: Option<i64>, run: Option<&str>, exp: i64, variant: Option<&str>| NewRequestFailure {
            user_id: uid,
            attribution_correlation_id: run.map(str::to_string),
            experiment_id: Some(exp),
            experiment_variant: variant.map(str::to_string),
            ..failure("req")
        };
        let mut created = BTreeMap::new();
        for (uid, run, exp, variant) in [
            (Some(alice), Some("a"), 7, Some("control")),
            (Some(alice), Some("a"), 7, Some("candidate")),
            (Some(alice), Some("a"), 7, Some("control")),
            (Some(bob), Some("a"), 7, Some("control")),
            // A missing variant sorts ahead of the named ones in its run.
            (Some(alice), Some("a"), 7, None),
            (Some(alice), Some("b"), 7, None),
            // Without a user or a correlation id there is no run to attach to.
            (None, Some("a"), 7, Some("control")),
            (Some(alice), None, 7, Some("control")),
            (Some(alice), Some("a"), 8, Some("control")),
        ] {
            let f = FailureRepository::create(&*db, row(uid, run, exp, variant)).await.unwrap();
            created
                .entry((uid, run.map(str::to_string), exp, variant.map(str::to_string)))
                .or_insert_with(Vec::new)
                .push(f.created_at);
            tick().await;
        }
        let span = |uid: i64, run: &str, variant: Option<&str>| {
            let ts = &created[&(Some(uid), Some(run.to_string()), 7, variant.map(str::to_string))];
            (ts.iter().min().unwrap().clone(), ts.iter().max().unwrap().clone())
        };

        let rows = db.experiment_run_failures(7).await.unwrap();
        let got: Vec<(i64, String, Option<String>, i64, String, String)> = rows
            .into_iter()
            .map(|r| (r.user_id, r.correlation_id, r.variant, r.failures, r.first_at, r.last_at))
            .collect();

        let entry = |uid: i64, run: &str, variant: Option<&str>, n: i64| {
            let (first, last) = span(uid, run, variant);
            (uid, run.to_string(), variant.map(str::to_string), n, first, last)
        };
        // Ordered by user, run, then variant (None first).
        let mut expected = vec![
            entry(alice, "a", None, 1),
            entry(alice, "a", Some("candidate"), 1),
            entry(alice, "a", Some("control"), 2),
            entry(alice, "b", None, 1),
            entry(bob, "a", Some("control"), 1),
        ];
        expected.sort_by(|x, y| (x.0, &x.1, &x.2).cmp(&(y.0, &y.1, &y.2)));
        assert_eq!(got, expected);
        let (first, last) = span(alice, "a", Some("control"));
        assert!(first < last);

        assert!(db.experiment_run_failures(99).await.unwrap().is_empty());
    })
    .await;
}

// ---- outcomes ----

#[tokio::test]
async fn outcome_upsert_replaces_the_earlier_report() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        let first = db.upsert(outcome(uid, "run-1", "failure", Some(9))).await.unwrap();
        assert_eq!(first.user_id, uid);
        assert_eq!(first.attribution_correlation_id, "run-1");
        assert_eq!(first.outcome, "failure");
        assert_eq!(first.score, Some(0.5));
        assert_eq!(first.rating, Some(3));
        assert_eq!(first.note.as_deref(), Some("first pass"));
        assert_eq!(first.experiment_id, Some(9));
        assert_eq!(first.experiment_variant.as_deref(), Some("control"));
        assert_eq!(first.created_at, first.updated_at);

        tick().await;
        let mut second = outcome(uid, "run-1", "success", Some(9));
        second.score = Some(0.9);
        second.rating = None;
        second.note = None;
        second.experiment_variant = Some("candidate".to_string());
        let replaced = db.upsert(second).await.unwrap();
        assert_eq!(replaced.outcome, "success");
        assert_eq!(replaced.score, Some(0.9));
        assert_eq!(replaced.rating, None);
        assert_eq!(replaced.note, None);
        assert_eq!(replaced.experiment_variant.as_deref(), Some("candidate"));
        assert_eq!(replaced.created_at, first.created_at);
        assert!(replaced.updated_at > first.updated_at);

        let got = OutcomeRepository::get(&*db, uid, "run-1").await.unwrap().unwrap();
        assert_eq!(got, replaced);
        assert_eq!(db.for_experiment(9).await.unwrap().len(), 1);

        // The replacement can also unstamp the run.
        let unstamped = db.upsert(outcome(uid, "run-1", "success", None)).await.unwrap();
        assert_eq!(unstamped.experiment_id, None);
        assert!(db.for_experiment(9).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn outcome_rows_are_keyed_by_user_and_correlation_id() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        db.upsert(outcome(alice, "run-2", "success", Some(9))).await.unwrap();
        db.upsert(outcome(alice, "run-1", "success", Some(9))).await.unwrap();
        db.upsert(outcome(bob, "run-1", "failure", Some(9))).await.unwrap();
        db.upsert(outcome(alice, "run-3", "failure", None)).await.unwrap();

        assert_eq!(OutcomeRepository::get(&*db, alice, "run-1").await.unwrap().unwrap().outcome, "success");
        assert_eq!(OutcomeRepository::get(&*db, bob, "run-1").await.unwrap().unwrap().outcome, "failure");
        assert!(OutcomeRepository::get(&*db, bob + 100, "run-1").await.unwrap().is_none());
        assert!(OutcomeRepository::get(&*db, alice, "run-9").await.unwrap().is_none());

        // Ordered by user, then correlation id.
        let for_exp: Vec<(i64, String)> = db
            .for_experiment(9)
            .await
            .unwrap()
            .into_iter()
            .map(|o| (o.user_id, o.attribution_correlation_id))
            .collect();
        let mut expected = vec![
            (alice, "run-1".to_string()),
            (alice, "run-2".to_string()),
            (bob, "run-1".to_string()),
        ];
        expected.sort();
        assert_eq!(for_exp, expected);
        assert!(db.for_experiment(8).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn outcome_clear_notes_only_touches_the_experiment() {
    each_backend(|db| async move {
        let uid = user(&db, "alice").await;
        db.upsert(outcome(uid, "run-1", "success", Some(9))).await.unwrap();
        db.upsert(outcome(uid, "run-2", "success", Some(9))).await.unwrap();
        db.upsert(outcome(uid, "run-3", "success", Some(8))).await.unwrap();
        db.upsert(outcome(uid, "run-4", "success", None)).await.unwrap();
        db.upsert(NewRunOutcome { note: None, ..outcome(uid, "run-5", "success", Some(9)) }).await.unwrap();

        assert_eq!(db.clear_notes(9).await.unwrap(), 2);
        assert!(OutcomeRepository::get(&*db, uid, "run-1").await.unwrap().unwrap().note.is_none());
        assert!(OutcomeRepository::get(&*db, uid, "run-2").await.unwrap().unwrap().note.is_none());
        assert_eq!(OutcomeRepository::get(&*db, uid, "run-3").await.unwrap().unwrap().note.as_deref(), Some("first pass"));
        assert_eq!(OutcomeRepository::get(&*db, uid, "run-4").await.unwrap().unwrap().note.as_deref(), Some("first pass"));
        // The rest of the row survives.
        let kept = OutcomeRepository::get(&*db, uid, "run-1").await.unwrap().unwrap();
        assert_eq!(kept.outcome, "success");
        assert_eq!(kept.score, Some(0.5));
        assert_eq!(kept.rating, Some(3));
        assert_eq!(db.clear_notes(9).await.unwrap(), 0);
    })
    .await;
}

// ---- experiments ----

#[tokio::test]
async fn experiment_create_then_get_round_trips_every_column() {
    each_backend(|db| async move {
        let created = ExperimentRepository::create(&*db, new_experiment("exp-a", 0)).await.unwrap();
        let got = ExperimentRepository::get(&*db, created.id).await.unwrap().expect("row exists");
        assert_eq!(got, created);

        assert_eq!(got.name, "exp-a");
        assert_eq!(got.variants, variants());
        assert_eq!(got.variants["candidate"]["fast"].target, "anthropic/claude-haiku");
        assert_eq!(got.allowed_user_ids, vec![3, 7]);
        assert_eq!(got.status, ExperimentStatus::Active);
        assert!(got.feed_learning);
        assert_eq!(got.expires_at, 0);
        assert!(chrono::DateTime::parse_from_rfc3339(&got.created_at).is_ok());
        assert!(got.closed_at.is_none());
        assert!(got.retain_content);
        assert_eq!(got.content_retention_days, 0);

        assert!(ExperimentRepository::get(&*db, created.id + 100).await.unwrap().is_none());

        // The other value of every flag, an open allow-list and a large expiry.
        let open = ExperimentRepository::create(&*db, NewExperiment {
            allowed_user_ids: vec![],
            feed_learning: false,
            retain_content: false,
            content_retention_days: 30,
            expires_at: 4_102_444_800,
            ..new_experiment("open", 0)
        })
        .await
        .unwrap();
        let open = ExperimentRepository::get(&*db, open.id).await.unwrap().unwrap();
        assert!(open.allowed_user_ids.is_empty());
        assert!(!open.feed_learning);
        assert!(!open.retain_content);
        assert_eq!(open.content_retention_days, 30);
        assert_eq!(open.expires_at, 4_102_444_800);
    })
    .await;
}

#[tokio::test]
async fn experiment_duplicate_name_is_an_error() {
    each_backend(|db| async move {
        ExperimentRepository::create(&*db, new_experiment("dup", 0)).await.unwrap();
        assert!(ExperimentRepository::create(&*db, new_experiment("dup", 0)).await.is_err());
        assert_eq!(ExperimentRepository::list(&*db, ExperimentStatusFilter::All).await.unwrap().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn experiment_list_filters_by_status_newest_first() {
    each_backend(|db| async move {
        assert!(ExperimentRepository::list(&*db, ExperimentStatusFilter::All).await.unwrap().is_empty());
        let a = ExperimentRepository::create(&*db, new_experiment("a", 0)).await.unwrap();
        let b = ExperimentRepository::create(&*db, new_experiment("b", 0)).await.unwrap();
        assert!(db.close(a.id, "2026-09-01T00:00:00+00:00").await.unwrap());

        let ids = |v: Vec<modelrouter::db::models::Experiment>| v.iter().map(|e| e.id).collect::<Vec<_>>();
        assert_eq!(ids(ExperimentRepository::list(&*db, ExperimentStatusFilter::All).await.unwrap()), vec![b.id, a.id]);
        assert_eq!(ids(ExperimentRepository::list(&*db, ExperimentStatusFilter::Active).await.unwrap()), vec![b.id]);
        let closed = ExperimentRepository::list(&*db, ExperimentStatusFilter::Closed).await.unwrap();
        assert_eq!(ids(closed.clone()), vec![a.id]);
        assert_eq!(closed[0].closed_at.as_deref(), Some("2026-09-01T00:00:00+00:00"));
        assert_eq!(closed[0].status, ExperimentStatus::Closed);
    })
    .await;
}

#[tokio::test]
async fn experiment_close_twice_changes_one_row_once() {
    each_backend(|db| async move {
        let e = ExperimentRepository::create(&*db, new_experiment("once", 0)).await.unwrap();
        assert!(db.close(e.id, "2026-09-01T00:00:00+00:00").await.unwrap());
        assert!(!db.close(e.id, "2026-09-02T00:00:00+00:00").await.unwrap());
        let got = ExperimentRepository::get(&*db, e.id).await.unwrap().unwrap();
        // The first close wins; the second did not touch the row.
        assert_eq!(got.closed_at.as_deref(), Some("2026-09-01T00:00:00+00:00"));
        assert!(!db.close(e.id + 100, "2026-09-01T00:00:00+00:00").await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn experiment_close_expired_closes_only_elapsed_nonzero_expiries() {
    each_backend(|db| async move {
        let create = |name: &'static str, expires_at: i64| {
            let db = db.clone();
            async move { ExperimentRepository::create(&*db, new_experiment(name, expires_at)).await.unwrap() }
        };
        let never = create("never", 0).await;
        let past = create("past", 1_000).await;
        let at_now = create("at-now", 2_000).await;
        let future = create("future", 3_000).await;
        let already = create("already", 500).await;
        assert!(db.close(already.id, "2026-08-01T00:00:00+00:00").await.unwrap());

        let mut closed = db.close_expired(2_000, "2026-09-01T00:00:00+00:00").await.unwrap();
        closed.sort();
        assert_eq!(closed, vec![past.id, at_now.id]);

        let get = |id: i64| {
            let db = db.clone();
            async move { ExperimentRepository::get(&*db, id).await.unwrap().unwrap() }
        };
        assert_eq!(get(never.id).await.status, ExperimentStatus::Active);
        assert_eq!(get(future.id).await.status, ExperimentStatus::Active);
        let past_row = get(past.id).await;
        assert_eq!(past_row.status, ExperimentStatus::Closed);
        assert_eq!(past_row.closed_at.as_deref(), Some("2026-09-01T00:00:00+00:00"));
        // The one closed earlier keeps its original close time.
        assert_eq!(get(already.id).await.closed_at.as_deref(), Some("2026-08-01T00:00:00+00:00"));

        // Nothing left to close.
        assert!(db.close_expired(2_000, "2026-09-02T00:00:00+00:00").await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn experiment_retaining_rows_by_window() {
    each_backend(|db| async move {
        let create = |new: NewExperiment| {
            let db = db.clone();
            async move { ExperimentRepository::create(&*db, new).await.unwrap() }
        };
        let plain = create(NewExperiment { retain_content: false, ..new_experiment("plain", 0) }).await;
        db.close(plain.id, "2026-08-31T00:00:00+00:00").await.unwrap();

        let active = create(new_experiment("active", 0)).await;
        let forever = create(new_experiment("forever", 0)).await;
        db.close(forever.id, "2026-01-01T00:00:00+00:00").await.unwrap();

        let windowed = create(NewExperiment { content_retention_days: 10, ..new_experiment("windowed", 0) }).await;
        db.close(windowed.id, "2026-08-25T00:00:00+00:00").await.unwrap();

        let elapsed = create(NewExperiment { content_retention_days: 10, ..new_experiment("elapsed", 0) }).await;
        db.close(elapsed.id, "2026-08-01T00:00:00+00:00").await.unwrap();

        let now = "2026-09-01T00:00:00+00:00";
        let open: Vec<i64> = db
            .all_retaining_open_or_within_window(now)
            .await
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(open, vec![active.id, forever.id, windowed.id]);

        // Exactly at the boundary the window has elapsed.
        let at_boundary: Vec<i64> = db
            .all_retaining_open_or_within_window("2026-09-04T00:00:00+00:00")
            .await
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(at_boundary, vec![active.id, forever.id]);

        assert!(db.all_retaining_open_or_within_window("not a timestamp").await.is_err());

        let closed: Vec<i64> = db.closed_retaining().await.unwrap().iter().map(|e| e.id).collect();
        assert_eq!(closed, vec![forever.id, windowed.id, elapsed.id]);
    })
    .await;
}
