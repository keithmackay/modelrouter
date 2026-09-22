use std::sync::Arc;
use arc_swap::ArcSwap;
use crate::{api::app::DatabaseProvider, db::models::User};
use crate::config::schema::Settings;
use crate::router::declarative_policy::find_matching_rule;

pub struct PolicyEngine {
    pub db: Arc<dyn DatabaseProvider>,
    settings: Option<Arc<ArcSwap<Settings>>>,
}

pub struct BudgetContext {
    pub limit_usd: f64,
    pub spent_usd: f64,
    pub window: String,
}

pub enum PolicyDecision {
    Allow {
        /// Most restrictive max_concurrent across all applicable budget rules.
        /// None means unlimited.
        max_concurrent: Option<u32>,
    },
    Deny {
        reason: String,
        status: u16,
        budget_context: Option<BudgetContext>,
    },
}

impl PolicyEngine {
    pub fn new(db: Arc<dyn DatabaseProvider>) -> Self {
        Self { db, settings: None }
    }

    /// Attach live settings for declarative policy rule evaluation.
    /// Call this on the production instance; test instances can skip it.
    pub fn with_settings(mut self, settings: Arc<ArcSwap<Settings>>) -> Self {
        self.settings = Some(settings);
        self
    }

    /// Response-cache participation for this caller/model (issue #30). A
    /// matched declarative rule with `cache = false` opts the caller out of
    /// the response cache — no lookup, no store. No rules, no match, or a
    /// rule without the field → enabled (the global cache policy decides).
    pub fn cache_enabled(&self, user: &crate::db::models::User, model: &str) -> bool {
        if let Some(ref live) = self.settings {
            let settings = live.load();
            if !settings.policy_rules.is_empty() {
                if let Some(rule) = find_matching_rule(&settings.policy_rules, user, model) {
                    return rule.cache.unwrap_or(true);
                }
            }
        }
        true
    }

    #[tracing::instrument(skip(self), fields(
        policy.result = tracing::field::Empty,
        policy.reason = tracing::field::Empty,
    ))]
    pub async fn check(&self, user: &User, model: &str) -> anyhow::Result<PolicyDecision> {
        use crate::db::models::BudgetScope;
        use crate::db::repositories::budgets::BudgetRepository;

        // ── Declarative policy rules (config-driven, highest priority) ──────
        if let Some(decision) = self.check_declarative_rules(user, model).await? {
            return Ok(decision);
        }
        // ── Fallthrough: existing database-driven rules ──────────────────────

        // Get budget rules for this user (user-specific first)
        let mut rules = BudgetRepository::list_for_user(&*self.db, user.id).await?;
        // Per-key rules take precedence — check them first by prepending
        if let Some(key_id) = user.api_key_id {
            let key_rules = BudgetRepository::list_for_key(&*self.db, key_id).await?;
            rules = key_rules.into_iter().chain(rules).collect();
        }
        // Note: tag-based budget rules (BudgetRule.tag) are a potential future addition.
        // api_key_tag has been replaced by api_key_project (project attribution, not bearer-embedded).

        let mut min_concurrent: Option<u32> = None;

        for rule in &rules {
            // Skip "target" window rules — these are informational group targets, not enforceable limits
            if rule.window == "target" {
                continue;
            }
            if let Some(decision) = check_model_lists(rule, model) {
                return Ok(decision);
            }
            if let Some(decision) = self.check_rate_limit(user, rule).await? {
                return Ok(decision);
            }
            if let Some(decision) = self.check_usd_budget(user, rule).await? {
                return Ok(decision);
            }
            // Track max_concurrent
            if let Some(mc) = rule.max_concurrent {
                let mc = mc.max(0) as u32;
                min_concurrent = Some(min_concurrent.map_or(mc, |prev| prev.min(mc)));
            }
            if let Some(decision) = self.check_token_budget(user, rule).await? {
                return Ok(decision);
            }
        }

        // Project and global rules are checked after user/key rules.
        // Most-specific rules (user/key) are enforced first; project and global
        // act as backstops that apply regardless of per-user limits.

        // ── Project-scope rules ──────────────────────────────────────────────────
        if let Some(proj) = user.api_key_project.as_deref() {
            let project_rules = BudgetRepository::list_for_scope(
                &*self.db,
                &BudgetScope::Project(proj.to_string()),
            )
            .await?;
            if let Some(decision) = self.check_backstop_rules(&project_rules, Some(proj)).await? {
                return Ok(decision);
            }
        }

        // ── Global rules ──────────────────────────────────────────────────────────
        let global_rules = BudgetRepository::list_for_scope(&*self.db, &BudgetScope::Global).await?;
        if let Some(decision) = self.check_backstop_rules(&global_rules, None).await? {
            return Ok(decision);
        }

        tracing::Span::current().record("policy.result", "allow");
        Ok(PolicyDecision::Allow { max_concurrent: min_concurrent })
    }

    /// Declarative (config-driven) rules. `Some(decision)` when a rule matched
    /// — allow or deny, either way the database rules are skipped; `None` to
    /// fall through to them.
    async fn check_declarative_rules(
        &self,
        user: &User,
        model: &str,
    ) -> anyhow::Result<Option<PolicyDecision>> {
        use crate::db::repositories::costs::CostRepository;

        let Some(ref live) = self.settings else {
            return Ok(None);
        };
        let settings = live.load();
        if settings.policy_rules.is_empty() {
            return Ok(None);
        }
        let Some(rule) = find_matching_rule(&settings.policy_rules, user, model) else {
            return Ok(None);
        };
        tracing::debug!(rule.name = rule.name.as_str(), "declarative policy rule matched");

        // 1. Model allow-list check
        if !rule.allow_models.is_empty() && !rule.allow_models.contains(&model.to_string()) {
            let reason = format!("model '{}' not permitted by policy rule '{}'", model, rule.name);
            return Ok(Some(deny(reason, 403, None)));
        }

        // 2. USD budget check
        if let Some(limit_usd) = rule.budget_usd {
            let window_start = window_start_for(&rule.window);
            let spent = CostRepository::sum_for_user_since(&*self.db, user.id, &window_start).await?;
            if spent >= limit_usd {
                let reason = format!(
                    "budget exceeded by policy rule '{}': ${:.4} of ${:.2} {} limit",
                    rule.name, spent, limit_usd, rule.window
                );
                let ctx = BudgetContext {
                    limit_usd,
                    spent_usd: spent,
                    window: rule.window.clone(),
                };
                return Ok(Some(deny(reason, 429, Some(ctx))));
            }
        }

        // Rule matched and all checks passed — allow, skip DB rules
        tracing::Span::current().record("policy.result", "allow");
        Ok(Some(PolicyDecision::Allow { max_concurrent: None }))
    }

    /// Rate limit (rate_rpm) — atomic increment+check to avoid TOCTOU race.
    async fn check_rate_limit(
        &self,
        user: &User,
        rule: &crate::db::models::BudgetRule,
    ) -> anyhow::Result<Option<PolicyDecision>> {
        use crate::db::repositories::rate_limits::RateLimitRepository;

        let Some(rate_rpm) = rule.rate_rpm else {
            return Ok(None);
        };
        let window_key = format!("rpm:{}", current_minute_bucket());
        let new_count =
            RateLimitRepository::increment_and_get_count(&*self.db, user.id, &window_key).await?;
        if new_count > rate_rpm {
            return Ok(Some(deny("rate limit exceeded".to_string(), 429, None)));
        }
        Ok(None)
    }

    /// USD budget for a user/key rule, over the rule's window (with the user's
    /// `spend_reset_at` honored for calendar windows).
    async fn check_usd_budget(
        &self,
        user: &User,
        rule: &crate::db::models::BudgetRule,
    ) -> anyhow::Result<Option<PolicyDecision>> {
        use crate::db::repositories::costs::CostRepository;

        let Some(limit_usd) = rule.limit_usd else {
            return Ok(None);
        };
        let spent = if rule.window == "total" {
            let start = rule.window_start.as_deref().unwrap_or("1970-01-01T00:00:00Z");
            let end = rule.window_end.as_deref().unwrap_or("9999-12-31T23:59:59Z");
            if let Some(key_id) = rule.api_key_id {
                // TODO: window_end not enforced for key-scoped total rules (no sum_for_key_between yet)
                CostRepository::sum_for_key_since(&*self.db, key_id, start).await?
            } else {
                CostRepository::sum_for_user_between(&*self.db, user.id, start, end).await?
            }
        } else {
            let window_start = effective_window_start(user, &rule.window);
            if let Some(key_id) = rule.api_key_id {
                CostRepository::sum_for_key_since(&*self.db, key_id, &window_start).await?
            } else {
                CostRepository::sum_for_user_since(&*self.db, user.id, &window_start).await?
            }
        };
        if spent >= limit_usd {
            let reason = format!(
                "budget exceeded: ${:.4} of ${:.2} {} limit",
                spent, limit_usd, rule.window
            );
            let ctx = BudgetContext {
                limit_usd,
                spent_usd: spent,
                window: rule.window.clone(),
            };
            return Ok(Some(deny(reason, 429, Some(ctx))));
        }
        Ok(None)
    }

    /// Token budget for a user/key rule (calendar windows only, with the
    /// user's `spend_reset_at` honored).
    async fn check_token_budget(
        &self,
        user: &User,
        rule: &crate::db::models::BudgetRule,
    ) -> anyhow::Result<Option<PolicyDecision>> {
        use crate::db::repositories::costs::CostRepository;

        let Some(limit_tokens) = rule.limit_tokens else {
            return Ok(None);
        };
        let window_start = effective_window_start(user, &rule.window);
        let used_tokens = if let Some(key_id) = rule.api_key_id {
            CostRepository::sum_tokens_for_key_since(&*self.db, key_id, &window_start).await?
        } else {
            CostRepository::sum_tokens_for_user_since(&*self.db, user.id, &window_start).await?
        };
        if used_tokens >= limit_tokens {
            let reason = format!(
                "token budget exceeded: {} of {} {} tokens used",
                used_tokens, limit_tokens, rule.window
            );
            return Ok(Some(deny(reason, 429, None)));
        }
        Ok(None)
    }

    /// Project (`proj = Some`) or global (`proj = None`) backstop rules.
    /// Only `limit_usd` is enforced at these scopes; limit_tokens,
    /// model_allow, model_deny and rate_rpm are stored but not yet enforced
    /// here — per-user/key rules cover those. `spend_reset_at` is not applied:
    /// these windows use a uniform calendar period, not per-entity resets.
    async fn check_backstop_rules(
        &self,
        rules: &[crate::db::models::BudgetRule],
        proj: Option<&str>,
    ) -> anyhow::Result<Option<PolicyDecision>> {
        use crate::db::repositories::costs::CostRepository;

        for rule in rules {
            let Some(limit_usd) = rule.limit_usd else {
                continue;
            };
            let spent = match rule.window.as_str() {
                "total" => {
                    let start = rule.window_start.as_deref().unwrap_or("1970-01-01T00:00:00Z");
                    let end = rule.window_end.as_deref().unwrap_or("9999-12-31T23:59:59Z");
                    match proj {
                        Some(p) => {
                            CostRepository::sum_for_project_between(&*self.db, p, start, end).await?
                        }
                        None => CostRepository::sum_global_between(&*self.db, start, end).await?,
                    }
                }
                _ => {
                    let since = window_start_for(&rule.window);
                    match proj {
                        Some(p) => {
                            CostRepository::sum_for_project_since(&*self.db, p, &since).await?
                        }
                        None => CostRepository::sum_global_since(&*self.db, &since).await?,
                    }
                }
            };
            if spent >= limit_usd {
                let reason = match proj {
                    Some(p) => format!(
                        "project budget exceeded: ${:.4} of ${:.2} {} limit for project '{}'",
                        spent, limit_usd, rule.window, p
                    ),
                    None => format!(
                        "global budget exceeded: ${:.4} of ${:.2} {} limit",
                        spent, limit_usd, rule.window
                    ),
                };
                let ctx = BudgetContext {
                    limit_usd,
                    spent_usd: spent,
                    window: rule.window.clone(),
                };
                return Ok(Some(deny(reason, 429, Some(ctx))));
            }
        }
        Ok(None)
    }

    /// Model allow/deny evaluation only, for failover re-checks: no rate-limit
    /// increment, no budget sums (neither is model-dependent). Returns
    /// Some(reason) when `model` is denied for this user, None when permitted.
    ///
    /// CRITICAL: This does NOT increment rate limits or sum budgets. The
    /// original `check()` already counted the request; re-running it for
    /// fallback candidates would double-count against rate_rpm. Budget sums
    /// are model-independent (a $10 monthly limit covers all models), so
    /// re-summing adds nothing. Failover only needs to know whether the
    /// SERVING model is permitted.
    pub async fn model_permitted_denial(&self, user: &User, model: &str) -> anyhow::Result<Option<String>> {
        use crate::db::repositories::budgets::BudgetRepository;

        // ── Declarative policy rules (config-driven, highest priority) ──────
        if let Some(ref live) = self.settings {
            let settings = live.load();
            if !settings.policy_rules.is_empty() {
                if let Some(rule) = find_matching_rule(&settings.policy_rules, user, model) {
                    // Model allow-list check
                    if !rule.allow_models.is_empty() && !rule.allow_models.contains(&model.to_string()) {
                        return Ok(Some(format!("model '{}' not permitted by policy rule '{}'", model, rule.name)));
                    }
                    // Rule matched and model is permitted — skip DB rules
                    return Ok(None);
                }
            }
        }

        // ── Fallthrough: existing database-driven rules ──────────────────────
        // Get budget rules for this user (user-specific first)
        let mut rules = BudgetRepository::list_for_user(&*self.db, user.id).await?;
        // Per-key rules take precedence — check them first by prepending
        if let Some(key_id) = user.api_key_id {
            let key_rules = BudgetRepository::list_for_key(&*self.db, key_id).await?;
            rules = key_rules.into_iter().chain(rules).collect();
        }

        for rule in &rules {
            // Skip "target" window rules — these are informational group targets, not enforceable limits
            if rule.window == "target" {
                continue;
            }
            if let Some(reason) = check_model_lists_denial(rule, model) {
                return Ok(Some(reason));
            }
        }

        Ok(None)
    }
}

/// Record the denial on the current policy span and build the decision.
fn deny(reason: String, status: u16, budget_context: Option<BudgetContext>) -> PolicyDecision {
    let span = tracing::Span::current();
    span.record("policy.result", "deny");
    span.record("policy.reason", reason.as_str());
    PolicyDecision::Deny { reason, status, budget_context }
}

/// Compute the model allow/deny denial reason without recording it on the span.
/// Returns Some(reason) when the model is denied, None when permitted.
fn check_model_lists_denial(
    rule: &crate::db::models::BudgetRule,
    model: &str,
) -> Option<String> {
    let model_allow: Vec<String> = serde_json::from_str(&rule.model_allow).unwrap_or_default();
    if !model_allow.is_empty() && !model_allow.contains(&model.to_string()) {
        return Some(format!("model '{}' not in allow list", model));
    }
    let model_deny: Vec<String> = serde_json::from_str(&rule.model_deny).unwrap_or_default();
    if model_deny.contains(&model.to_string()) {
        return Some(format!("model '{}' is denied", model));
    }
    None
}

/// Model allow/deny lists on a user/key rule (JSON arrays).
fn check_model_lists(
    rule: &crate::db::models::BudgetRule,
    model: &str,
) -> Option<PolicyDecision> {
    check_model_lists_denial(rule, model).map(|reason| deny(reason, 403, None))
}

/// Window start with the user's `spend_reset_at` honored: whichever is later.
/// SAFETY: both strings are produced by chrono::to_rfc3339() with UTC offset
/// +00:00 (via now_utc()). Lexicographic ordering is correct when the format
/// is consistent. Do not mix with Z-suffix or fractional-second timestamps
/// from external sources.
fn effective_window_start(user: &User, window: &str) -> String {
    let raw = window_start_for(window);
    match &user.spend_reset_at {
        Some(reset_at) if reset_at.as_str() > raw.as_str() => reset_at.clone(),
        _ => raw,
    }
}

fn current_minute_bucket() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use crate::db::models::{BudgetScope, NewBudgetRule, NewUser};
    use crate::db::sqlite::SqliteDb;
    use crate::db::repositories::budgets::BudgetRepository;
    use crate::db::repositories::users::UserRepository;
    use crate::db::models::User;
    use crate::api::app::DatabaseProvider;
    use crate::router::policy::{PolicyDecision, PolicyEngine};
    use std::sync::Arc;

    async fn make_db() -> Arc<SqliteDb> {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        Arc::new(db)
    }

    async fn make_user(db: &SqliteDb) -> User {
        UserRepository::create(db, NewUser {
            name: "alice".to_string(),
            email: None,
        }).await.unwrap()
    }

    async fn insert_spend(db: &SqliteDb, user_id: i64, cost: f64) {
        // Insert a prompt first (prompt_id NOT NULL in cost_ledger)
        let prompt_id: i64 = sqlx::query_scalar(
            "INSERT INTO prompts (user_id, session_id, request_model, routed_model, provider, \
             messages, response, finish_reason, prompt_tokens, completion_tokens, cost_usd, \
             latency_ms, tags, project, created_at) \
             VALUES (?, NULL, 'test', 'test', 'test', '[]', NULL, NULL, 0, 0, ?, NULL, '[]', NULL, ?) \
             RETURNING id"
        )
        .bind(user_id)
        .bind(cost)
        .bind(chrono::Utc::now().to_rfc3339())
        .fetch_one(&db.pool).await.unwrap();

        sqlx::query(
            "INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project, \
             tokens_in, tokens_out, cost_usd, api_key_id, created_at) \
             VALUES (?, ?, 'test', 'test', NULL, 0, 0, ?, NULL, ?)"
        )
        .bind(user_id)
        .bind(prompt_id)
        .bind(cost)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&db.pool).await.unwrap();
    }

    #[tokio::test]
    async fn global_monthly_rule_blocks_when_exceeded() {
        let db = make_db().await;
        let user = make_user(&db).await;

        // Add spend directly so the global sum > limit
        insert_spend(&db, user.id, 200.0).await;

        // Global rule: $100/month
        BudgetRepository::create(&*db, NewBudgetRule {
            user_id: None, group_name: None, api_key_id: None, tag: None, project: None,
            window: "monthly".to_string(),
            limit_usd: Some(100.0),
            limit_tokens: None, model_allow: vec![], model_deny: vec![],
            rate_rpm: None, max_concurrent: None, window_start: None, window_end: None,
        }).await.unwrap();

        let engine = PolicyEngine::new(db.clone() as Arc<dyn DatabaseProvider>);
        let decision = engine.check(&user, "claude-sonnet-4-6").await.unwrap();
        assert!(matches!(decision, PolicyDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn group_target_rule_does_not_block() {
        let db = make_db().await;
        let user = make_user(&db).await;

        // Add lots of spend
        insert_spend(&db, user.id, 9999.0).await;

        // Group target rule — should NOT enforce (window="target" is skipped)
        BudgetRepository::create(&*db, NewBudgetRule {
            user_id: None, group_name: Some("engineering".to_string()), api_key_id: None,
            tag: None, project: None,
            window: "target".to_string(),
            limit_usd: Some(1.0),
            limit_tokens: None, model_allow: vec![], model_deny: vec![],
            rate_rpm: None, max_concurrent: None, window_start: None, window_end: None,
        }).await.unwrap();

        let engine = PolicyEngine::new(db.clone() as Arc<dyn DatabaseProvider>);
        let decision = engine.check(&user, "claude-sonnet-4-6").await.unwrap();
        assert!(matches!(decision, PolicyDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn model_permitted_denial_denies_model_not_in_allow_list() {
        let db = make_db().await;
        let user = make_user(&db).await;

        BudgetRepository::create(&*db, NewBudgetRule {
            user_id: Some(user.id),
            group_name: None, api_key_id: None, tag: None, project: None,
            window: "monthly".to_string(),
            limit_usd: None,
            limit_tokens: None,
            rate_rpm: None,
            max_concurrent: None,
            model_allow: vec!["gpt-4o".to_string()],
            model_deny: vec![],
            window_start: None,
            window_end: None,
        }).await.unwrap();

        let engine = PolicyEngine::new(db.clone() as Arc<dyn DatabaseProvider>);
        let denial = engine.model_permitted_denial(&user, "claude-sonnet-4-6").await.unwrap();
        assert!(denial.is_some());
        assert!(denial.unwrap().contains("not in allow list"));
    }

    #[tokio::test]
    async fn model_permitted_denial_permits_model_in_allow_list() {
        let db = make_db().await;
        let user = make_user(&db).await;

        BudgetRepository::create(&*db, NewBudgetRule {
            user_id: Some(user.id),
            group_name: None, api_key_id: None, tag: None, project: None,
            window: "monthly".to_string(),
            limit_usd: None,
            limit_tokens: None,
            rate_rpm: None,
            max_concurrent: None,
            model_allow: vec!["gpt-4o".to_string(), "claude-sonnet-4-6".to_string()],
            model_deny: vec![],
            window_start: None,
            window_end: None,
        }).await.unwrap();

        let engine = PolicyEngine::new(db.clone() as Arc<dyn DatabaseProvider>);
        let denial = engine.model_permitted_denial(&user, "claude-sonnet-4-6").await.unwrap();
        assert!(denial.is_none());
    }

    #[tokio::test]
    async fn model_permitted_denial_permits_when_no_lists() {
        let db = make_db().await;
        let user = make_user(&db).await;

        BudgetRepository::create(&*db, NewBudgetRule {
            user_id: Some(user.id),
            group_name: None, api_key_id: None, tag: None, project: None,
            window: "monthly".to_string(),
            limit_usd: Some(10.0),
            limit_tokens: None,
            rate_rpm: None,
            max_concurrent: None,
            model_allow: vec![],
            model_deny: vec![],
            window_start: None,
            window_end: None,
        }).await.unwrap();

        let engine = PolicyEngine::new(db.clone() as Arc<dyn DatabaseProvider>);
        let denial = engine.model_permitted_denial(&user, "any-model").await.unwrap();
        assert!(denial.is_none());
    }
}

fn window_start_for(window: &str) -> String {
    let now = chrono::Utc::now();
    match window {
        "daily" => now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .to_rfc3339(),
        "weekly" => {
            use chrono::Datelike;
            let days_since_monday = now.weekday().num_days_from_monday() as i64;
            (now - chrono::Duration::days(days_since_monday))
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .to_rfc3339()
        }
        _ => {
            // monthly
            use chrono::Datelike;
            now.with_day(1)
                .unwrap()
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .to_rfc3339()
        }
    }
}
