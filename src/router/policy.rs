use std::sync::Arc;
use crate::{api::app::DatabaseProvider, db::models::User};

pub struct PolicyEngine {
    pub db: Arc<dyn DatabaseProvider>,
}

pub enum PolicyDecision {
    Allow,
    Deny { reason: String, status: u16 },
}

impl PolicyEngine {
    pub fn new(db: Arc<dyn DatabaseProvider>) -> Self {
        Self { db }
    }

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

        for rule in &rules {
            // 1. Check model_allow (JSON array)
            let model_allow: Vec<String> =
                serde_json::from_str(&rule.model_allow).unwrap_or_default();
            if !model_allow.is_empty() && !model_allow.contains(&model.to_string()) {
                return Ok(PolicyDecision::Deny {
                    reason: format!("model '{}' not in allow list", model),
                    status: 403,
                });
            }

            // 2. Check model_deny
            let model_deny: Vec<String> =
                serde_json::from_str(&rule.model_deny).unwrap_or_default();
            if model_deny.contains(&model.to_string()) {
                return Ok(PolicyDecision::Deny {
                    reason: format!("model '{}' is denied", model),
                    status: 403,
                });
            }

            // 3. Check rate limit (rate_rpm)
            if let Some(rate_rpm) = rule.rate_rpm {
                let window_key = format!("rpm:{}", current_minute_bucket());
                let count =
                    RateLimitRepository::get_request_count(&*self.db, user.id, &window_key)
                        .await?;
                if count >= rate_rpm {
                    return Ok(PolicyDecision::Deny {
                        reason: "rate limit exceeded".to_string(),
                        status: 429,
                    });
                }
                // Increment the counter (fire-and-forget)
                RateLimitRepository::increment_request_count(&*self.db, user.id, &window_key)
                    .await
                    .ok();
            }

            // 4. Check budget
            if let Some(limit_usd) = rule.limit_usd {
                let window_start = window_start_for(&rule.window);
                let spent =
                    CostRepository::sum_for_user_since(&*self.db, user.id, &window_start).await?;
                if spent >= limit_usd {
                    return Ok(PolicyDecision::Deny {
                        reason: format!(
                            "budget exceeded: ${:.4} of ${:.2} {} limit",
                            spent, limit_usd, rule.window
                        ),
                        status: 429,
                    });
                }
            }
        }

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
