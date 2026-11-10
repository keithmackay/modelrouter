//! Conformance scenarios: catalog repositories.

#[allow(unused_imports)]
use crate::harness::each_backend;

use std::sync::Arc;

use modelrouter::api::app::DatabaseProvider;
use modelrouter::db::models::{
    BudgetScope, LearnedModelCapability, NewApiKey, NewBudgetRule, NewModel, NewModelAlias,
    NewUser, UpdateBudgetRule,
};
use modelrouter::db::repositories::budgets::BudgetRepository;
use modelrouter::db::repositories::users::UserRepository;

async fn user(db: &Arc<dyn DatabaseProvider>, name: &str) -> i64 {
    UserRepository::create(&**db, NewUser { name: name.into(), email: None })
        .await
        .unwrap()
        .id
}

async fn key(db: &Arc<dyn DatabaseProvider>, user_id: i64, hash: &str) -> i64 {
    db.create_api_key(NewApiKey {
        user_id,
        key_hash: hash.into(),
        label: None,
        expires_at: None,
        project: None,
        session_window_secs: None,
    })
    .await
    .unwrap()
    .id
}

fn rule(window: &str) -> NewBudgetRule {
    NewBudgetRule {
        user_id: None,
        group_name: None,
        api_key_id: None,
        tag: None,
        window: window.into(),
        limit_usd: None,
        limit_tokens: None,
        model_allow: vec![],
        model_deny: vec![],
        rate_rpm: None,
        max_concurrent: None,
        project: None,
        window_start: None,
        window_end: None,
    }
}

fn no_changes() -> UpdateBudgetRule {
    UpdateBudgetRule {
        limit_usd: None,
        limit_tokens: None,
        model_allow: None,
        model_deny: None,
        rate_rpm: None,
        max_concurrent: None,
        window_start: None,
        window_end: None,
    }
}

fn ids(rules: &[modelrouter::db::models::BudgetRule]) -> Vec<i64> {
    let mut out: Vec<i64> = rules.iter().map(|r| r.id).collect();
    out.sort();
    out
}

// ── models ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn models_create_get_list_delete() {
    each_backend(|db| async move {
        let b = db
            .create_model(NewModel { provider: "openai".into(), name: "gpt-x".into(), alias: None })
            .await
            .unwrap();
        let a = db
            .create_model(NewModel {
                provider: "anthropic".into(),
                name: "claude-x".into(),
                alias: Some("fast".into()),
            })
            .await
            .unwrap();
        assert!(a.enabled);
        assert_eq!(a.alias.as_deref(), Some("fast"));
        assert!(a.disabled_reason.is_none() && a.disabled_by.is_none() && a.disabled_at.is_none());
        assert!(!a.created_at.is_empty());

        let got = db.get_model(a.id).await.unwrap().unwrap();
        assert_eq!((got.provider.as_str(), got.name.as_str()), ("anthropic", "claude-x"));
        assert!(db.get_model(a.id + b.id + 100).await.unwrap().is_none());

        // Ordered by provider, then name.
        let names: Vec<String> =
            db.list_models().await.unwrap().into_iter().map(|m| m.name).collect();
        assert_eq!(names, ["claude-x", "gpt-x"]);

        // Aliases are unique.
        assert!(db
            .create_model(NewModel {
                provider: "openai".into(),
                name: "other".into(),
                alias: Some("fast".into()),
            })
            .await
            .is_err());

        assert!(db.delete_model(a.id).await.unwrap());
        assert!(!db.delete_model(a.id).await.unwrap());
        assert_eq!(db.list_models().await.unwrap().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn models_enable_disable_records_reason() {
    each_backend(|db| async move {
        let m = db
            .create_model(NewModel { provider: "openai".into(), name: "gpt-x".into(), alias: None })
            .await
            .unwrap();

        db.set_model_enabled_with_reason(m.id, false, Some("flaky"), Some("ops"))
            .await
            .unwrap();
        let off = db.get_model(m.id).await.unwrap().unwrap();
        assert!(!off.enabled);
        assert_eq!(off.disabled_reason.as_deref(), Some("flaky"));
        assert_eq!(off.disabled_by.as_deref(), Some("ops"));
        assert!(off.disabled_at.is_some());

        db.set_model_enabled_with_reason(m.id, true, Some("ignored"), Some("ops"))
            .await
            .unwrap();
        let on = db.get_model(m.id).await.unwrap().unwrap();
        assert!(on.enabled);
        assert!(on.disabled_reason.is_none() && on.disabled_by.is_none() && on.disabled_at.is_none());

        db.set_model_enabled(m.id, false).await.unwrap();
        let off = db.get_model(m.id).await.unwrap().unwrap();
        assert!(!off.enabled);
        assert!(off.disabled_reason.is_none() && off.disabled_by.is_none());
        assert!(off.disabled_at.is_some());
        db.set_model_enabled(m.id, true).await.unwrap();
        assert!(db.get_model(m.id).await.unwrap().unwrap().enabled);
    })
    .await;
}

#[tokio::test]
async fn models_failover_chains() {
    each_backend(|db| async move {
        assert!(db.list_failovers("a").await.unwrap().is_empty());

        db.set_failovers("b", &["b1".into()]).await.unwrap();
        db.set_failovers("a", &["x".into(), "y".into(), "z".into()]).await.unwrap();
        let chain = db.list_failovers("a").await.unwrap();
        let got: Vec<(String, i64)> =
            chain.iter().map(|f| (f.fallback_model.clone(), f.priority)).collect();
        assert_eq!(got, [("x".into(), 0), ("y".into(), 1), ("z".into(), 2)]);
        assert!(chain.iter().all(|f| f.primary_model == "a"));

        // Replacing the chain drops the old rows.
        db.set_failovers("a", &["z".into(), "x".into()]).await.unwrap();
        let got: Vec<String> =
            db.list_failovers("a").await.unwrap().into_iter().map(|f| f.fallback_model).collect();
        assert_eq!(got, ["z", "x"]);

        // Ordered by primary, then priority.
        let all: Vec<(String, String)> = db
            .list_all_failovers()
            .await
            .unwrap()
            .into_iter()
            .map(|f| (f.primary_model, f.fallback_model))
            .collect();
        assert_eq!(
            all,
            [("a".into(), "z".into()), ("a".into(), "x".into()), ("b".into(), "b1".into())]
        );

        db.set_failovers("a", &[]).await.unwrap();
        assert!(db.list_failovers("a").await.unwrap().is_empty());
        assert_eq!(db.list_all_failovers().await.unwrap().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn models_provider_states() {
    each_backend(|db| async move {
        assert!(db.list_provider_states().await.unwrap().is_empty());
        assert!(db.get_provider_state("openai").await.unwrap().is_none());

        db.set_provider_enabled("openai", false, Some("outage"), Some("ops")).await.unwrap();
        db.set_provider_enabled("anthropic", true, Some("ignored"), None).await.unwrap();

        let off = db.get_provider_state("openai").await.unwrap().unwrap();
        assert!(!off.enabled);
        assert_eq!(off.disabled_reason.as_deref(), Some("outage"));
        assert_eq!(off.disabled_by.as_deref(), Some("ops"));
        assert!(off.disabled_at.is_some());
        assert!(!off.updated_at.is_empty());

        let on = db.get_provider_state("anthropic").await.unwrap().unwrap();
        assert!(on.enabled);
        assert!(on.disabled_reason.is_none() && on.disabled_by.is_none() && on.disabled_at.is_none());

        // Re-enabling overwrites the row and clears the disable metadata.
        db.set_provider_enabled("openai", true, None, None).await.unwrap();
        let states = db.list_provider_states().await.unwrap();
        let names: Vec<&str> = states.iter().map(|s| s.provider.as_str()).collect();
        assert_eq!(names, ["anthropic", "openai"]);
        assert!(states[1].enabled && states[1].disabled_reason.is_none());
    })
    .await;
}

// ── groups ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn groups_create_find_list_priority() {
    each_backend(|db| async move {
        let low = db.create_group("low", 1).await.unwrap();
        let high = db.create_group("high", 10).await.unwrap();
        let tie = db.create_group("tie", 1).await.unwrap();
        assert!(low.enabled);
        assert_eq!(low.priority, 1);
        assert!(!low.created_at.is_empty());

        assert!(db.create_group("low", 5).await.is_err(), "group names are unique");

        assert_eq!(db.get_group(high.id).await.unwrap().unwrap().name, "high");
        assert!(db.get_group(tie.id + 100).await.unwrap().is_none());
        assert_eq!(db.find_group_by_name("tie").await.unwrap().unwrap().id, tie.id);
        assert!(db.find_group_by_name("nope").await.unwrap().is_none());

        // Priority descending, then id ascending.
        let order: Vec<i64> = db.list_groups().await.unwrap().into_iter().map(|g| g.id).collect();
        assert_eq!(order, [high.id, low.id, tie.id]);

        db.set_group_priority(tie.id, 50).await.unwrap();
        assert_eq!(db.get_group(tie.id).await.unwrap().unwrap().priority, 50);
        let order: Vec<i64> = db.list_groups().await.unwrap().into_iter().map(|g| g.id).collect();
        assert_eq!(order, [tie.id, high.id, low.id]);
    })
    .await;
}

#[tokio::test]
async fn groups_memberships() {
    each_backend(|db| async move {
        let g = db.create_group("eng", 0).await.unwrap();
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;

        let m = db.add_member(g.id, alice).await.unwrap();
        assert_eq!((m.group_id, m.user_id), (g.id, alice));
        assert_eq!(m.user_name, "alice");
        assert!(m.disabled_at.is_none());
        assert!(!m.joined_at.is_empty());
        let mb = db.add_member(g.id, bob).await.unwrap();
        assert_eq!(mb.user_name, "bob");

        let active = db.find_active_membership(g.id, alice).await.unwrap().unwrap();
        assert_eq!(active.id, m.id);
        assert_eq!(active.user_name, "alice");

        db.disable_membership(m.id).await.unwrap();
        assert!(db.find_active_membership(g.id, alice).await.unwrap().is_none());
        let first_disable = db.list_memberships(g.id).await.unwrap()[0].disabled_at.clone();
        assert!(first_disable.is_some());
        // Disabling again leaves the original timestamp alone.
        db.disable_membership(m.id).await.unwrap();
        assert_eq!(db.list_memberships(g.id).await.unwrap()[0].disabled_at, first_disable);

        // Rejoining creates a fresh membership; history is kept, oldest first.
        let again = db.add_member(g.id, alice).await.unwrap();
        let all = db.list_memberships(g.id).await.unwrap();
        let got: Vec<(i64, &str, bool)> = all
            .iter()
            .map(|m| (m.id, m.user_name.as_str(), m.disabled_at.is_some()))
            .collect();
        assert_eq!(got, [(m.id, "alice", true), (mb.id, "bob", false), (again.id, "alice", false)]);

        let other = db.create_group("other", 0).await.unwrap();
        assert!(db.list_memberships(other.id).await.unwrap().is_empty());
        assert!(db.find_active_membership(other.id, bob).await.unwrap().is_none());
    })
    .await;
}

#[tokio::test]
async fn groups_disabling_disables_active_memberships() {
    each_backend(|db| async move {
        let g = db.create_group("eng", 0).await.unwrap();
        let keep = db.create_group("keep", 0).await.unwrap();
        let alice = user(&db, "alice").await;
        db.add_member(g.id, alice).await.unwrap();
        db.add_member(keep.id, alice).await.unwrap();

        db.set_group_enabled(g.id, false).await.unwrap();
        assert!(!db.get_group(g.id).await.unwrap().unwrap().enabled);
        assert!(db.find_active_membership(g.id, alice).await.unwrap().is_none());
        assert!(db.find_active_membership(keep.id, alice).await.unwrap().is_some());

        // Re-enabling the group does not revive memberships.
        db.set_group_enabled(g.id, true).await.unwrap();
        assert!(db.get_group(g.id).await.unwrap().unwrap().enabled);
        assert!(db.find_active_membership(g.id, alice).await.unwrap().is_none());
    })
    .await;
}

// ── budgets ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn budgets_create_round_trips_every_field() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let mut r = rule("monthly");
        r.user_id = Some(alice);
        r.limit_usd = Some(12.5);
        r.limit_tokens = Some(5_000_000_000);
        r.model_allow = vec!["a/*".into(), "b".into()];
        r.model_deny = vec!["c".into()];
        r.rate_rpm = Some(60);
        r.max_concurrent = Some(4);
        r.window_start = Some("2026-01-01T00:00:00Z".into());
        r.window_end = Some("2026-02-01T00:00:00Z".into());
        let created = BudgetRepository::create(&*db, r).await.unwrap();

        assert_eq!(created.user_id, Some(alice));
        assert!(created.group_name.is_none() && created.api_key_id.is_none());
        assert!(created.tag.is_none() && created.project.is_none());
        assert_eq!(created.window, "monthly");
        assert_eq!(created.limit_usd, Some(12.5));
        assert_eq!(created.limit_tokens, Some(5_000_000_000));
        assert_eq!(created.model_allow, r#"["a/*","b"]"#);
        assert_eq!(created.model_deny, r#"["c"]"#);
        assert_eq!(created.rate_rpm, Some(60));
        assert_eq!(created.max_concurrent, Some(4));
        assert_eq!(created.window_start.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(created.window_end.as_deref(), Some("2026-02-01T00:00:00Z"));
        assert!(!created.created_at.is_empty());
        assert_eq!(created.created_at, created.updated_at);

        let listed = db.list_all().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].model_allow, created.model_allow);
        assert_eq!(listed[0].rate_rpm, Some(60));
    })
    .await;
}

#[tokio::test]
async fn budgets_list_by_scope() {
    each_backend(|db| async move {
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;
        let k = key(&db, alice, "hash-alice").await;

        let global = BudgetRepository::create(&*db, rule("daily")).await.unwrap();
        let mut r = rule("monthly");
        r.user_id = Some(alice);
        let u1 = BudgetRepository::create(&*db, r).await.unwrap();
        let mut r = rule("daily");
        r.user_id = Some(alice);
        let u2 = BudgetRepository::create(&*db, r).await.unwrap();
        let mut r = rule("monthly");
        r.group_name = Some("eng".into());
        let grp = BudgetRepository::create(&*db, r).await.unwrap();
        let mut r = rule("monthly");
        r.api_key_id = Some(k);
        let per_key = BudgetRepository::create(&*db, r).await.unwrap();
        let mut r = rule("monthly");
        r.project = Some("proj".into());
        let proj = BudgetRepository::create(&*db, r).await.unwrap();
        let mut r = rule("monthly");
        r.tag = Some("batch".into());
        let tagged = BudgetRepository::create(&*db, r).await.unwrap();

        assert_eq!(ids(&db.list_for_user(alice).await.unwrap()), [u1.id, u2.id]);
        assert!(db.list_for_user(bob).await.unwrap().is_empty());
        assert_eq!(ids(&db.list_for_group("eng").await.unwrap()), [grp.id]);
        assert!(db.list_for_group("nope").await.unwrap().is_empty());
        assert_eq!(ids(&db.list_for_key(k).await.unwrap()), [per_key.id]);
        assert!(db.list_for_key(k + 100).await.unwrap().is_empty());
        assert_eq!(ids(&db.list_for_tag("batch").await.unwrap()), [tagged.id]);
        assert!(db.list_for_tag("nope").await.unwrap().is_empty());
        assert_eq!(
            ids(&db.list_all().await.unwrap()),
            [global.id, u1.id, u2.id, grp.id, per_key.id, proj.id, tagged.id]
        );

        // Global means no user, group, project or key; a tag-only rule counts.
        assert_eq!(
            ids(&db.list_for_scope(&BudgetScope::Global).await.unwrap()),
            [global.id, tagged.id]
        );
        assert_eq!(
            ids(&db.list_for_scope(&BudgetScope::Project("proj".into())).await.unwrap()),
            [proj.id]
        );
        assert_eq!(
            ids(&db.list_for_scope(&BudgetScope::User(alice)).await.unwrap()),
            [u1.id, u2.id]
        );
        assert_eq!(
            ids(&db.list_for_scope(&BudgetScope::Group("eng".into())).await.unwrap()),
            [grp.id]
        );
        assert!(db.list_for_scope(&BudgetScope::User(bob)).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn budgets_update_and_delete() {
    each_backend(|db| async move {
        let mut r = rule("monthly");
        r.limit_usd = Some(10.0);
        r.limit_tokens = Some(100);
        r.rate_rpm = Some(5);
        r.model_allow = vec!["a".into()];
        let created = BudgetRepository::create(&*db, r).await.unwrap();

        // An empty change set only bumps updated_at.
        let same = BudgetRepository::update(&*db, created.id, &no_changes()).await.unwrap();
        assert_eq!(same.limit_usd, Some(10.0));
        assert_eq!(same.limit_tokens, Some(100));
        assert_eq!(same.rate_rpm, Some(5));
        assert_eq!(same.model_allow, r#"["a"]"#);

        let changed = BudgetRepository::update(
            &*db,
            created.id,
            &UpdateBudgetRule {
                limit_usd: Some(20.0),
                limit_tokens: Some(200),
                model_allow: Some(vec![]),
                model_deny: Some(vec!["x".into(), "y".into()]),
                rate_rpm: Some(30),
                max_concurrent: Some(2),
                window_start: Some("2026-03-01T00:00:00Z".into()),
                window_end: Some("2026-04-01T00:00:00Z".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(changed.id, created.id);
        assert_eq!(changed.window, "monthly");
        assert_eq!(changed.limit_usd, Some(20.0));
        assert_eq!(changed.limit_tokens, Some(200));
        assert_eq!(changed.model_allow, "[]");
        assert_eq!(changed.model_deny, r#"["x","y"]"#);
        assert_eq!(changed.rate_rpm, Some(30));
        assert_eq!(changed.max_concurrent, Some(2));
        assert_eq!(changed.window_start.as_deref(), Some("2026-03-01T00:00:00Z"));
        assert_eq!(changed.window_end.as_deref(), Some("2026-04-01T00:00:00Z"));
        assert_eq!(changed.created_at, created.created_at);

        let err = BudgetRepository::update(&*db, created.id + 100, &no_changes())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");

        BudgetRepository::delete(&*db, created.id).await.unwrap();
        assert!(db.list_all().await.unwrap().is_empty());
        // Deleting a missing rule is not an error.
        BudgetRepository::delete(&*db, created.id).await.unwrap();
    })
    .await;
}

// ── aliases ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn aliases_upsert_get_list_delete() {
    each_backend(|db| async move {
        assert!(db.list_aliases().await.unwrap().is_empty());
        assert!(db.get_alias("fast").await.unwrap().is_none());

        let created = db
            .upsert_alias(NewModelAlias {
                alias: "fast".into(),
                target: "openai/gpt-x".into(),
                created_by: Some("alice".into()),
            })
            .await
            .unwrap();
        assert_eq!(created.target, "openai/gpt-x");
        assert_eq!(created.created_by.as_deref(), Some("alice"));
        assert_eq!(created.created_at, created.updated_at);

        db.upsert_alias(NewModelAlias {
            alias: "cheap".into(),
            target: "anthropic/haiku".into(),
            created_by: None,
        })
        .await
        .unwrap();

        // Replacing the target keeps the original author and creation time.
        let replaced = db
            .upsert_alias(NewModelAlias {
                alias: "fast".into(),
                target: "anthropic/sonnet".into(),
                created_by: Some("bob".into()),
            })
            .await
            .unwrap();
        assert_eq!(replaced.target, "anthropic/sonnet");
        assert_eq!(replaced.created_by.as_deref(), Some("alice"));
        assert_eq!(replaced.created_at, created.created_at);

        let got = db.get_alias("fast").await.unwrap().unwrap();
        assert_eq!(got.target, "anthropic/sonnet");

        let names: Vec<String> =
            db.list_aliases().await.unwrap().into_iter().map(|a| a.alias).collect();
        assert_eq!(names, ["cheap", "fast"]);

        assert!(db.delete_alias("fast").await.unwrap());
        assert!(!db.delete_alias("fast").await.unwrap());
        assert!(db.get_alias("fast").await.unwrap().is_none());
        assert_eq!(db.list_aliases().await.unwrap().len(), 1);
    })
    .await;
}

// ── learned capabilities ─────────────────────────────────────────────────────

#[tokio::test]
async fn learned_capabilities_upsert_list_delete() {
    each_backend(|db| async move {
        assert!(db.list_learned_capabilities().await.unwrap().is_empty());

        let b = LearnedModelCapability {
            model: "b-model".into(),
            supports_temperature: false,
            error: "temperature unsupported".into(),
            learned_at: "2026-01-01T00:00:00Z".into(),
        };
        let a = LearnedModelCapability {
            model: "a-model@2026".into(),
            supports_temperature: true,
            error: "".into(),
            learned_at: "2026-01-02T00:00:00Z".into(),
        };
        db.upsert_learned_capability(&b).await.unwrap();
        db.upsert_learned_capability(&a).await.unwrap();
        assert_eq!(db.list_learned_capabilities().await.unwrap(), [a.clone(), b.clone()]);

        // Upserting the same model replaces every field.
        let b2 = LearnedModelCapability {
            supports_temperature: true,
            error: "relearned".into(),
            learned_at: "2026-02-01T00:00:00Z".into(),
            ..b.clone()
        };
        db.upsert_learned_capability(&b2).await.unwrap();
        assert_eq!(db.list_learned_capabilities().await.unwrap(), [a.clone(), b2]);

        assert!(db.delete_learned_capability("b-model").await.unwrap());
        assert!(!db.delete_learned_capability("b-model").await.unwrap());
        assert_eq!(db.list_learned_capabilities().await.unwrap(), [a]);
    })
    .await;
}
