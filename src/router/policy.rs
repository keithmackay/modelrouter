use std::sync::Arc;
use crate::{api::app::DatabaseProvider, db::models::User};

pub struct PolicyEngine {
    pub db: Arc<dyn DatabaseProvider>,
}

pub struct BudgetContext {
    pub limit_usd: f64,
    pub spent_usd: f64,
    pub window: String,
}

pub enum PolicyDecision {
    Allow,
    Deny {
        reason: String,
        status: u16,
        budget_context: Option<BudgetContext>,
    },
}

impl PolicyEngine {
    pub fn new(db: Arc<dyn DatabaseProvider>) -> Self {
        Self { db }
    }

    #[tracing::instrument(skip(self), fields(
        policy.result = tracing::field::Empty,
        policy.reason = tracing::field::Empty,
    ))]
    pub async fn check(&self, user: &User, model: &str) -> anyhow::Result<PolicyDecision> {
        use crate::db::repositories::budgets::BudgetRepository;
        use crate::db::repositories::costs::CostRepository;
        use crate::db::repositories::rate_limits::RateLimitRepository;

        // Get budget rules for this user (user-specific first, then group)
        let mut rules = BudgetRepository::list_for_user(&*self.db, user.id).await?;
        if let Some(ref group) = user.group_name {
            let group_rules = BudgetRepository::list_for_group(&*self.db, group).await?;
            rules.extend(group_rules);
        }

        let span = tracing::Span::current();

        for rule in &rules {
            // 1. Check model_allow (JSON array)
            let model_allow: Vec<String> =
                serde_json::from_str(&rule.model_allow).unwrap_or_default();
            if !model_allow.is_empty() && !model_allow.contains(&model.to_string()) {
                let reason = format!("model '{}' not in allow list", model);
                span.record("policy.result", "deny");
                span.record("policy.reason", reason.as_str());
                return Ok(PolicyDecision::Deny {
                    reason,
                    status: 403,
                    budget_context: None,
                });
            }

            // 2. Check model_deny
            let model_deny: Vec<String> =
                serde_json::from_str(&rule.model_deny).unwrap_or_default();
            if model_deny.contains(&model.to_string()) {
                let reason = format!("model '{}' is denied", model);
                span.record("policy.result", "deny");
                span.record("policy.reason", reason.as_str());
                return Ok(PolicyDecision::Deny {
                    reason,
                    status: 403,
                    budget_context: None,
                });
            }

            // 3. Check rate limit (rate_rpm) — atomic increment+check to avoid TOCTOU race
            if let Some(rate_rpm) = rule.rate_rpm {
                let window_key = format!("rpm:{}", current_minute_bucket());
                let new_count =
                    RateLimitRepository::increment_and_get_count(&*self.db, user.id, &window_key)
                        .await?;
                if new_count > rate_rpm {
                    let reason = "rate limit exceeded".to_string();
                    span.record("policy.result", "deny");
                    span.record("policy.reason", reason.as_str());
                    return Ok(PolicyDecision::Deny {
                        reason,
                        status: 429,
                        budget_context: None,
                    });
                }
            }

            // 4. Check budget
            if let Some(limit_usd) = rule.limit_usd {
                let window_start = window_start_for(&rule.window);
                let spent =
                    CostRepository::sum_for_user_since(&*self.db, user.id, &window_start).await?;
                if spent >= limit_usd {
                    let reason = format!(
                        "budget exceeded: ${:.4} of ${:.2} {} limit",
                        spent, limit_usd, rule.window
                    );
                    span.record("policy.result", "deny");
                    span.record("policy.reason", reason.as_str());
                    return Ok(PolicyDecision::Deny {
                        reason,
                        status: 429,
                        budget_context: Some(BudgetContext {
                            limit_usd,
                            spent_usd: spent,
                            window: rule.window.clone(),
                        }),
                    });
                }
            }
        }

        span.record("policy.result", "allow");
        Ok(PolicyDecision::Allow)
    }
}

fn current_minute_bucket() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M").to_string()
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
