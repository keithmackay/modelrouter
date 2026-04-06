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

    #[tracing::instrument(skip(self), fields(
        policy.result = tracing::field::Empty,
        policy.reason = tracing::field::Empty,
    ))]
    pub async fn check(&self, user: &User, model: &str) -> anyhow::Result<PolicyDecision> {
        use crate::db::repositories::budgets::BudgetRepository;
        use crate::db::repositories::costs::CostRepository;
        use crate::db::repositories::rate_limits::RateLimitRepository;

        let span = tracing::Span::current();

        // ── Declarative policy rules (config-driven, highest priority) ──────
        if let Some(ref live) = self.settings {
            let settings = live.load();
            if !settings.policy_rules.is_empty() {
                if let Some(rule) = find_matching_rule(&settings.policy_rules, user, model) {
                    tracing::debug!(rule.name = rule.name.as_str(), "declarative policy rule matched");

                    // 1. Model allow-list check
                    if !rule.allow_models.is_empty() && !rule.allow_models.contains(&model.to_string()) {
                        let reason = format!("model '{}' not permitted by policy rule '{}'", model, rule.name);
                        span.record("policy.result", "deny");
                        span.record("policy.reason", reason.as_str());
                        return Ok(PolicyDecision::Deny { reason, status: 403, budget_context: None });
                    }

                    // 2. USD budget check
                    if let Some(limit_usd) = rule.budget_usd {
                        use crate::db::repositories::costs::CostRepository;
                        let window_start = window_start_for(&rule.window);
                        let spent = CostRepository::sum_for_user_since(&*self.db, user.id, &window_start).await?;
                        if spent >= limit_usd {
                            let reason = format!(
                                "budget exceeded by policy rule '{}': ${:.4} of ${:.2} {} limit",
                                rule.name, spent, limit_usd, rule.window
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

                    // Rule matched and all checks passed — allow, skip DB rules
                    span.record("policy.result", "allow");
                    return Ok(PolicyDecision::Allow { max_concurrent: None });
                }
            }
        }
        // ── Fallthrough: existing database-driven rules ──────────────────────

        // Get budget rules for this user (user-specific first, then group)
        let mut rules = BudgetRepository::list_for_user(&*self.db, user.id).await?;
        if let Some(ref group) = user.group_name {
            let group_rules = BudgetRepository::list_for_group(&*self.db, group).await?;
            rules.extend(group_rules);
        }
        // Per-key rules take precedence — check them first by prepending
        if let Some(key_id) = user.api_key_id {
            let key_rules = BudgetRepository::list_for_key(&*self.db, key_id).await?;
            rules = key_rules.into_iter().chain(rules).collect();
        }
        // Include budget rules targeting this key's tag (lowest priority — appended last)
        if let Some(tag) = &user.api_key_tag {
            let tag_rules = self.db.list_for_tag(tag).await.unwrap_or_default();
            rules.extend(tag_rules);
        }

        let mut min_concurrent: Option<u32> = None;

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

            // 4. Check USD budget
            if let Some(limit_usd) = rule.limit_usd {
                let raw_window_start = window_start_for(&rule.window);
                // Honor spend_reset_at: use whichever timestamp is later
                // SAFETY: both strings are produced by chrono::to_rfc3339() with UTC offset +00:00
                // (via now_utc()). Lexicographic ordering is correct when format is consistent.
                // Do not mix with Z-suffix or fractional-second timestamps from external sources.
                let window_start = match &user.spend_reset_at {
                    Some(reset_at) if reset_at.as_str() > raw_window_start.as_str() => reset_at.clone(),
                    _ => raw_window_start,
                };
                let spent = if let Some(key_id) = rule.api_key_id {
                    CostRepository::sum_for_key_since(&*self.db, key_id, &window_start).await?
                } else {
                    CostRepository::sum_for_user_since(&*self.db, user.id, &window_start).await?
                };
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

            // 6. Track max_concurrent
            if let Some(mc) = rule.max_concurrent {
                let mc = mc.max(0) as u32;
                min_concurrent = Some(min_concurrent.map_or(mc, |prev| prev.min(mc)));
            }

            // 5. Check token budget
            if let Some(limit_tokens) = rule.limit_tokens {
                let raw_window_start = window_start_for(&rule.window);
                // SAFETY: both strings are produced by chrono::to_rfc3339() with UTC offset +00:00
                // (via now_utc()). Lexicographic ordering is correct when format is consistent.
                // Do not mix with Z-suffix or fractional-second timestamps from external sources.
                let window_start = match &user.spend_reset_at {
                    Some(reset_at) if reset_at.as_str() > raw_window_start.as_str() => reset_at.clone(),
                    _ => raw_window_start,
                };
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
                    span.record("policy.result", "deny");
                    span.record("policy.reason", reason.as_str());
                    return Ok(PolicyDecision::Deny { reason, status: 429, budget_context: None });
                }
            }
        }

        span.record("policy.result", "allow");
        Ok(PolicyDecision::Allow { max_concurrent: min_concurrent })
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
