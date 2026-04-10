use crate::config::schema::{PolicyConditionConfig, PolicyRuleConfig};
use crate::db::models::User;

/// Returns true if ALL non-None fields in `condition` match the given user and model.
/// An all-None condition matches everything.
pub fn condition_matches(condition: &PolicyConditionConfig, user: &User, model: &str) -> bool {
    if let Some(tag) = &condition.tag {
        if user.api_key_project.as_deref() != Some(tag.as_str()) {
            return false;
        }
    }
    if let Some(uid) = condition.user_id {
        if user.id != uid {
            return false;
        }
    }
    if let Some(m) = &condition.model {
        if model != m.as_str() {
            return false;
        }
    }
    true
}

/// Returns the highest-priority rule whose condition matches `user` and `model`.
pub fn find_matching_rule<'a>(
    rules: &'a [PolicyRuleConfig],
    user: &User,
    model: &str,
) -> Option<&'a PolicyRuleConfig> {
    let mut sorted: Vec<&PolicyRuleConfig> = rules.iter().collect();
    sorted.sort_by(|a, b| b.priority.cmp(&a.priority));
    sorted.into_iter().find(|r| condition_matches(&r.condition, user, model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{PolicyConditionConfig, PolicyRuleConfig};
    use crate::db::models::User;

    fn user(tag: Option<&str>, id: i64) -> User {
        User {
            id,
            name: "test".to_string(),
            email: None,
            enabled: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            metadata: "{}".to_string(),
            api_key_id: None,
            spend_reset_at: None,
            api_key_project: tag.map(str::to_string),
        }
    }

    fn rule(name: &str, priority: i32, condition: PolicyConditionConfig) -> PolicyRuleConfig {
        PolicyRuleConfig {
            name: name.to_string(),
            condition,
            allow_models: vec![],
            budget_usd: None,
            window: "monthly".to_string(),
            priority,
        }
    }

    #[test]
    fn empty_condition_matches_all() {
        let u = user(None, 1);
        let r = rule("open", 0, PolicyConditionConfig::default());
        assert!(condition_matches(&r.condition, &u, "gpt-4o"));
    }

    #[test]
    fn tag_condition_matches_exact() {
        let u = user(Some("research"), 1);
        let cond = PolicyConditionConfig { tag: Some("research".to_string()), ..Default::default() };
        assert!(condition_matches(&cond, &u, "gpt-4o"));
    }

    #[test]
    fn tag_condition_rejects_wrong_tag() {
        let u = user(Some("intern"), 1);
        let cond = PolicyConditionConfig { tag: Some("research".to_string()), ..Default::default() };
        assert!(!condition_matches(&cond, &u, "gpt-4o"));
    }

    #[test]
    fn tag_condition_rejects_no_tag() {
        let u = user(None, 1);
        let cond = PolicyConditionConfig { tag: Some("research".to_string()), ..Default::default() };
        assert!(!condition_matches(&cond, &u, "gpt-4o"));
    }

    #[test]
    fn user_id_condition_matches() {
        let u = user(None, 42);
        let cond = PolicyConditionConfig { user_id: Some(42), ..Default::default() };
        assert!(condition_matches(&cond, &u, "gpt-4o"));
    }

    #[test]
    fn model_condition_matches() {
        let u = user(None, 1);
        let cond = PolicyConditionConfig { model: Some("claude-opus-4-5".to_string()), ..Default::default() };
        assert!(condition_matches(&cond, &u, "claude-opus-4-5"));
        assert!(!condition_matches(&cond, &u, "gpt-4o"));
    }

    #[test]
    fn multiple_conditions_all_must_match() {
        // Both tag AND user_id must match
        let u_match = user(Some("research"), 42);
        let u_wrong_tag = user(Some("intern"), 42);
        let u_wrong_id = user(Some("research"), 99);
        let cond = PolicyConditionConfig {
            tag: Some("research".to_string()),
            user_id: Some(42),
            ..Default::default()
        };
        assert!(condition_matches(&cond, &u_match, "gpt-4o"));
        assert!(!condition_matches(&cond, &u_wrong_tag, "gpt-4o"));
        assert!(!condition_matches(&cond, &u_wrong_id, "gpt-4o"));
    }

    #[test]
    fn matching_rule_picks_highest_priority() {
        let u = user(Some("research"), 1);
        let rules = vec![
            rule("low", 1, PolicyConditionConfig { tag: Some("research".to_string()), ..Default::default() }),
            rule("high", 10, PolicyConditionConfig { tag: Some("research".to_string()), ..Default::default() }),
        ];
        let found = find_matching_rule(&rules, &u, "gpt-4o");
        assert_eq!(found.map(|r| r.name.as_str()), Some("high"));
    }

    #[test]
    fn matching_rule_returns_none_when_no_match() {
        let u = user(None, 1);
        let rules = vec![
            rule("tag-only", 5, PolicyConditionConfig { tag: Some("research".to_string()), ..Default::default() }),
        ];
        assert!(find_matching_rule(&rules, &u, "gpt-4o").is_none());
    }

    #[test]
    fn matching_rule_returns_none_for_empty_rules() {
        let u = user(None, 1);
        assert!(find_matching_rule(&[], &u, "gpt-4o").is_none());
    }
}
