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
            api_key_id: None,
            tag: None,
            window: window.to_string(),
            limit_usd: Some(limit_usd),
            limit_tokens: None,
            rate_rpm: None,
            max_concurrent: None,
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
    assert!(matches!(decision, PolicyDecision::Allow { .. }));
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
            api_key_id: None,
            tag: None,
            window: "monthly".to_string(),
            limit_usd: None,
            limit_tokens: None,
            rate_rpm: None,
            max_concurrent: None,
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
            api_key_id: None,
            tag: None,
            window: "monthly".to_string(),
            limit_usd: None,
            limit_tokens: None,
            rate_rpm: None,
            max_concurrent: None,
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
    assert!(matches!(decision, PolicyDecision::Allow { .. }));
}

#[tokio::test]
async fn budget_exceeded_returns_deny() {
    let db = common::in_memory_db().await;
    use modelrouter::db::repositories::{
        budgets::BudgetRepository, costs::CostRepository, prompts::PromptRepository,
    };
    use modelrouter::db::models::{NewBudgetRule, NewCostLedgerEntry, NewPrompt};
    use modelrouter::api::auth::hash_token;

    // Create user with $0.01 budget
    let user = UserRepository::create(
        &db,
        NewUser {
            name: "budget-exceeded-test".to_string(),
            api_key_hash: hash_token("budget-exceeded-test"),
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
            api_key_id: None,
            tag: None,
            window: "monthly".to_string(),
            limit_usd: Some(0.01),
            limit_tokens: None,
            rate_rpm: None,
            max_concurrent: None,
            model_allow: vec![],
            model_deny: vec![],
        },
    )
    .await
    .unwrap();

    // Create a prompt so we have a valid prompt_id for the cost ledger entry
    let prompt = PromptRepository::create(
        &db,
        NewPrompt {
            user_id: user.id,
            session_id: None,
            request_model: "gpt-4o".to_string(),
            routed_model: "gpt-4o".to_string(),
            provider: "openai".to_string(),
            messages: "[]".to_string(),
            response: None,
            finish_reason: None,
            prompt_tokens: 1000,
            completion_tokens: 500,
            cost_usd: 1.0,
            latency_ms: None,
            tags: "[]".to_string(),
            project: None,
        },
    )
    .await
    .unwrap();

    // Record cost exceeding limit
    CostRepository::create(
        &db,
        NewCostLedgerEntry {
            user_id: user.id,
            prompt_id: prompt.id,
            model: "gpt-4o".to_string(),
            provider: "openai".to_string(),
            project: None,
            tokens_in: 1000,
            tokens_out: 500,
            cost_usd: 1.0, // Way over the $0.01 limit
            api_key_id: None,
        },
    )
    .await
    .unwrap();

    let engine = PolicyEngine::new(Arc::new(db));
    let decision = engine.check(&user, "gpt-4o").await.unwrap();
    assert!(
        matches!(decision, PolicyDecision::Deny { status: 429, .. }),
        "should deny over-budget user"
    );
}

#[tokio::test]
async fn test_policy_token_limit_under_budget() {
    let db = common::in_memory_db().await;
    use modelrouter::db::repositories::{
        budgets::BudgetRepository, costs::CostRepository, prompts::PromptRepository,
    };
    use modelrouter::db::models::{NewBudgetRule, NewCostLedgerEntry, NewPrompt};
    use modelrouter::api::auth::hash_token;

    let user = UserRepository::create(
        &db,
        NewUser {
            name: format!("token-under-{}", uuid::Uuid::new_v4()),
            api_key_hash: hash_token(&uuid::Uuid::new_v4().to_string()),
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
            api_key_id: None,
            tag: None,
            window: "monthly".to_string(),
            limit_usd: None,
            limit_tokens: Some(100),
            rate_rpm: None,
            max_concurrent: None,
            model_allow: vec![],
            model_deny: vec![],
        },
    )
    .await
    .unwrap();

    let prompt = PromptRepository::create(
        &db,
        NewPrompt {
            user_id: user.id,
            session_id: None,
            request_model: "gpt-4o".to_string(),
            routed_model: "gpt-4o".to_string(),
            provider: "openai".to_string(),
            messages: "[]".to_string(),
            response: None,
            finish_reason: None,
            prompt_tokens: 60,
            completion_tokens: 35,
            cost_usd: 0.001,
            latency_ms: None,
            tags: "[]".to_string(),
            project: None,
        },
    )
    .await
    .unwrap();

    // Insert 60 + 35 = 95 tokens — under the 100 token limit
    CostRepository::create(
        &db,
        NewCostLedgerEntry {
            user_id: user.id,
            prompt_id: prompt.id,
            model: "gpt-4o".to_string(),
            provider: "openai".to_string(),
            project: None,
            tokens_in: 60,
            tokens_out: 35,
            cost_usd: 0.001,
            api_key_id: None,
        },
    )
    .await
    .unwrap();

    let engine = PolicyEngine::new(Arc::new(db));
    let decision = engine.check(&user, "gpt-4o").await.unwrap();
    assert!(
        matches!(decision, PolicyDecision::Allow { .. }),
        "95 tokens used with 100 token limit should allow"
    );
}

#[tokio::test]
async fn test_policy_token_limit_blocks_when_exceeded() {
    let db = common::in_memory_db().await;
    use modelrouter::db::repositories::{
        budgets::BudgetRepository, costs::CostRepository, prompts::PromptRepository,
    };
    use modelrouter::db::models::{NewBudgetRule, NewCostLedgerEntry, NewPrompt};
    use modelrouter::api::auth::hash_token;

    let user = UserRepository::create(
        &db,
        NewUser {
            name: format!("token-exceeded-{}", uuid::Uuid::new_v4()),
            api_key_hash: hash_token(&uuid::Uuid::new_v4().to_string()),
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
            api_key_id: None,
            tag: None,
            window: "monthly".to_string(),
            limit_usd: None,
            limit_tokens: Some(50),
            rate_rpm: None,
            max_concurrent: None,
            model_allow: vec![],
            model_deny: vec![],
        },
    )
    .await
    .unwrap();

    let prompt = PromptRepository::create(
        &db,
        NewPrompt {
            user_id: user.id,
            session_id: None,
            request_model: "gpt-4o".to_string(),
            routed_model: "gpt-4o".to_string(),
            provider: "openai".to_string(),
            messages: "[]".to_string(),
            response: None,
            finish_reason: None,
            prompt_tokens: 40,
            completion_tokens: 20,
            cost_usd: 0.001,
            latency_ms: None,
            tags: "[]".to_string(),
            project: None,
        },
    )
    .await
    .unwrap();

    // Insert 40 + 20 = 60 tokens — over the 50 token limit
    CostRepository::create(
        &db,
        NewCostLedgerEntry {
            user_id: user.id,
            prompt_id: prompt.id,
            model: "gpt-4o".to_string(),
            provider: "openai".to_string(),
            project: None,
            tokens_in: 40,
            tokens_out: 20,
            cost_usd: 0.001,
            api_key_id: None,
        },
    )
    .await
    .unwrap();

    let engine = PolicyEngine::new(Arc::new(db));
    let decision = engine.check(&user, "gpt-4o").await.unwrap();
    assert!(
        matches!(decision, PolicyDecision::Deny { status: 429, .. }),
        "60 tokens used with 50 token limit should deny"
    );
}
