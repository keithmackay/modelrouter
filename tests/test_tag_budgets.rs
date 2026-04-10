// tests/test_tag_budgets.rs
// Note: api_keys.tag has been replaced by api_keys.project for per-project cost attribution.
// BudgetRule.tag is retained as a potential future addition for tag-based budget rules.

#[test]
fn api_key_project_field_compiles() {
    let _key = modelrouter::db::models::NewApiKey {
        user_id: 1,
        key_hash: "abc".to_string(),
        label: None,
        expires_at: None,
        project: Some("my-project".to_string()),
    };
}

#[test]
fn budget_rule_tag_field_compiles() {
    let _rule = modelrouter::db::models::NewBudgetRule {
        user_id: None,
        group_name: None,
        api_key_id: None,
        tag: Some("ci".to_string()),
        window: "monthly".to_string(),
        limit_usd: None,
        limit_tokens: None,
        model_allow: vec![],
        model_deny: vec![],
        rate_rpm: None,
        max_concurrent: None,
    };
}

#[test]
fn user_has_api_key_project_field() {
    let user = modelrouter::db::models::User {
        id: 1,
        name: "test".to_string(),
        email: None,
        enabled: true,
        created_at: "2026-01-01T00:00:00+00:00".to_string(),
        metadata: "{}".to_string(),
        api_key_id: None,
        spend_reset_at: None,
        api_key_project: None,
    };
    assert!(user.api_key_project.is_none());
}
