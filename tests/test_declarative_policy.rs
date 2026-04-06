//! Integration tests for PolicyEngine with declarative (config-driven) rules.

mod common;

use std::sync::Arc;
use arc_swap::ArcSwap;
use modelrouter::{
    config::schema::{Settings, PolicyRuleConfig, PolicyConditionConfig},
    db::migrations::run_migrations,
    db::sqlite::SqliteDb,
    router::policy::{PolicyDecision, PolicyEngine},
};
use modelrouter::db::models::User;
use modelrouter::api::app::DatabaseProvider;

async fn test_db() -> Arc<dyn DatabaseProvider> {
    let db = SqliteDb::connect(":memory:").await.unwrap();
    run_migrations(&db.pool).await.unwrap();
    Arc::new(db)
}

fn test_user(tag: Option<&str>) -> User {
    User {
        id: 1,
        name: "test-user".to_string(),
        api_key: "hashed-key".to_string(),
        api_key_old: None,
        api_key_old_expires_at: None,
        group_name: None,
        enabled: true,
        created_at: "2026-01-01T00:00:00+00:00".to_string(),
        metadata: "{}".to_string(),
        api_key_id: None,
        spend_reset_at: None,
        api_key_tag: tag.map(|s| s.to_string()),
    }
}

fn settings_with_rules(rules: Vec<PolicyRuleConfig>) -> Arc<ArcSwap<Settings>> {
    let mut s = Settings::default();
    s.policy_rules = rules;
    Arc::new(ArcSwap::from_pointee(s))
}

// ── Test 1: declarative rule denies a model not in allow_models ──────────────

#[tokio::test]
async fn declarative_rule_denies_disallowed_model() {
    let db = test_db().await;
    let rule = PolicyRuleConfig {
        name: "research-only-opus".to_string(),
        condition: PolicyConditionConfig {
            tag: Some("research".to_string()),
            ..Default::default()
        },
        allow_models: vec!["claude-opus-4-5".to_string()],
        budget_usd: None,
        window: "monthly".to_string(),
        priority: 10,
    };
    let settings = settings_with_rules(vec![rule]);
    let engine = PolicyEngine::new(db).with_settings(settings);
    let user = test_user(Some("research"));

    let decision = engine.check(&user, "gpt-4o").await.unwrap();
    match decision {
        PolicyDecision::Deny { status, .. } => assert_eq!(status, 403),
        PolicyDecision::Allow { .. } => panic!("expected Deny, got Allow"),
    }
}

// ── Test 2: declarative rule allows a model in allow_models ──────────────────

#[tokio::test]
async fn declarative_rule_allows_permitted_model() {
    let db = test_db().await;
    let rule = PolicyRuleConfig {
        name: "research-only-opus".to_string(),
        condition: PolicyConditionConfig {
            tag: Some("research".to_string()),
            ..Default::default()
        },
        allow_models: vec!["claude-opus-4-5".to_string()],
        budget_usd: None,
        window: "monthly".to_string(),
        priority: 10,
    };
    let settings = settings_with_rules(vec![rule]);
    let engine = PolicyEngine::new(db).with_settings(settings);
    let user = test_user(Some("research"));

    let decision = engine.check(&user, "claude-opus-4-5").await.unwrap();
    assert!(matches!(decision, PolicyDecision::Allow { .. }));
}

// ── Test 3: declarative rule allows when spend is under budget ────────────────

#[tokio::test]
async fn declarative_rule_allows_under_budget() {
    let db = test_db().await;
    // Empty condition matches all users; fresh DB has zero spend
    let rule = PolicyRuleConfig {
        name: "global-budget".to_string(),
        condition: PolicyConditionConfig::default(),
        allow_models: vec![],
        budget_usd: Some(100.0),
        window: "monthly".to_string(),
        priority: 5,
    };
    let settings = settings_with_rules(vec![rule]);
    let engine = PolicyEngine::new(db).with_settings(settings);
    let user = test_user(None);

    let decision = engine.check(&user, "gpt-4o").await.unwrap();
    assert!(matches!(decision, PolicyDecision::Allow { .. }));
}

// ── Test 4: no matching rule falls through to DB rules (no DB rules → Allow) ──

#[tokio::test]
async fn no_matching_rule_falls_through_to_db_rules() {
    let db = test_db().await;
    // Rule only matches "premium" tag; user has "research" tag
    let rule = PolicyRuleConfig {
        name: "premium-only".to_string(),
        condition: PolicyConditionConfig {
            tag: Some("premium".to_string()),
            ..Default::default()
        },
        allow_models: vec!["gpt-4o".to_string()],
        budget_usd: None,
        window: "monthly".to_string(),
        priority: 10,
    };
    let settings = settings_with_rules(vec![rule]);
    let engine = PolicyEngine::new(db).with_settings(settings);
    let user = test_user(Some("research")); // does NOT match the "premium" rule

    // No DB budget rules exist for this user → falls through to Allow
    let decision = engine.check(&user, "gpt-4o").await.unwrap();
    assert!(matches!(decision, PolicyDecision::Allow { .. }));
}

// ── Test 5: empty policy_rules vec falls through to DB rules (→ Allow) ────────

#[tokio::test]
async fn empty_policy_rules_falls_through_to_db_rules() {
    let db = test_db().await;
    let settings = settings_with_rules(vec![]);
    let engine = PolicyEngine::new(db).with_settings(settings);
    let user = test_user(None);

    let decision = engine.check(&user, "claude-opus-4-5").await.unwrap();
    assert!(matches!(decision, PolicyDecision::Allow { .. }));
}
