mod common;

use modelrouter::db::repositories::{budgets::BudgetRepository, users::UserRepository};
use modelrouter::db::models::{NewBudgetRule, NewUser};
use modelrouter::router::policy::{PolicyDecision, PolicyEngine};
use std::sync::Arc;

async fn make_user_with_budget(
    db: &modelrouter::db::sqlite::SqliteDb,
    limit_usd: f64,
    window: &str,
) -> modelrouter::db::models::User {
    use modelrouter::api::auth::hash_token;
    let user = UserRepository::create(
        db,
        NewUser {
            name: format!("test-user-{}", uuid::Uuid::new_v4()),
            api_key_hash: hash_token(&uuid::Uuid::new_v4().to_string()),
            group_name: None,
        },
    )
    .await
    .unwrap();
    BudgetRepository::create(
        db,
        NewBudgetRule {
            user_id: Some(user.id),
            group_name: None,
            window: window.to_string(),
            limit_usd: Some(limit_usd),
            limit_tokens: None,
            rate_rpm: None,
            model_allow: vec![],
            model_deny: vec![],
        },
    )
    .await
    .unwrap();
    user
}

#[tokio::test]
async fn under_budget_allows_request() {
    let db = common::in_memory_db().await;
    let user = make_user_with_budget(&db, 10.0, "monthly").await;
    let engine = PolicyEngine::new(Arc::new(db));
    let decision = engine.check(&user, "gpt-4o").await.unwrap();
    assert!(matches!(decision, PolicyDecision::Allow));
}

#[tokio::test]
async fn model_in_deny_list_returns_403() {
    let db = common::in_memory_db().await;
    use modelrouter::api::auth::hash_token;
    let user = UserRepository::create(
        &db,
        NewUser {
            name: "deny-test".to_string(),
            api_key_hash: hash_token("deny-test"),
            group_name: None,
        },
    )
    .await
    .unwrap();
    BudgetRepository::create(
        &db,
        NewBudgetRule {
            user_id: Some(user.id),
            group_name: None,
            window: "monthly".to_string(),
            limit_usd: None,
            limit_tokens: None,
            rate_rpm: None,
            model_allow: vec![],
            model_deny: vec!["gpt-4".to_string()],
        },
    )
    .await
    .unwrap();
    let engine = PolicyEngine::new(Arc::new(db));
    let decision = engine.check(&user, "gpt-4").await.unwrap();
    assert!(matches!(decision, PolicyDecision::Deny { status: 403, .. }));
}

#[tokio::test]
async fn model_not_in_allow_list_returns_403() {
    let db = common::in_memory_db().await;
    use modelrouter::api::auth::hash_token;
    let user = UserRepository::create(
        &db,
        NewUser {
            name: "allow-test".to_string(),
            api_key_hash: hash_token("allow-test"),
            group_name: None,
        },
    )
    .await
    .unwrap();
    BudgetRepository::create(
        &db,
        NewBudgetRule {
            user_id: Some(user.id),
            group_name: None,
            window: "monthly".to_string(),
            limit_usd: None,
            limit_tokens: None,
            rate_rpm: None,
            model_allow: vec!["claude-haiku-4-5".to_string()],
            model_deny: vec![],
        },
    )
    .await
    .unwrap();
    let engine = PolicyEngine::new(Arc::new(db));
    let decision = engine.check(&user, "gpt-4o").await.unwrap();
    assert!(matches!(decision, PolicyDecision::Deny { status: 403, .. }));
}

#[tokio::test]
async fn no_budget_rules_allows_request() {
    let db = common::in_memory_db().await;
    use modelrouter::api::auth::hash_token;
    let user = UserRepository::create(
        &db,
        NewUser {
            name: "no-rules".to_string(),
            api_key_hash: hash_token("no-rules-token"),
            group_name: None,
        },
    )
    .await
    .unwrap();
    let engine = PolicyEngine::new(Arc::new(db));
    let decision = engine.check(&user, "gpt-4o").await.unwrap();
    assert!(matches!(decision, PolicyDecision::Allow));
}
