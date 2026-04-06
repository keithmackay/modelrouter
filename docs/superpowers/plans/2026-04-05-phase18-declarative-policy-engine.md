# Phase 18: Declarative Policy Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a TOML-configured, priority-ordered policy rule system that is evaluated before the existing database-driven budget rules, enabling operators to express access policies declaratively without touching the database.

**Architecture:** New `[[policy.rules]]` TOML section in Settings holds `PolicyRuleConfig` structs (each with a `condition` sub-table and enforcement fields). A pure `DeclarativePolicyEvaluator` module handles condition matching (no I/O). `PolicyEngine` gains an optional `settings` field via a builder method (preserving the existing `new(db)` signature so all test files compile unchanged). On each `check()` call, the engine loads live settings, finds the first matching config rule (priority descending), enforces it (model allow-list + USD budget), and returns early — bypassing DB rules. If no config rule matches, the existing DB rule path runs unchanged.

**Tech Stack:** Rust, serde/toml (already used), arc-swap (already in AppState), chrono (already used)

**Intentional spec deviations (documented):**
- Spec uses `[[policy.rules]]` (nested under a `[policy]` table); plan uses `[[policy_rules]]` (flat top-level array) — avoids a `[policy]` wrapper struct, simpler schema. Operators must use `[[policy_rules]]` not `[[policy.rules]]` in their config files.

---

## File Map

| Action | Path | Responsibility |
|--------|------|----------------|
| Modify | `src/config/schema.rs` | Add `PolicyConditionConfig`, `PolicyRuleConfig`; add `policy_rules` field to `Settings` |
| Create | `src/router/declarative_policy.rs` | Pure condition-matching logic + unit tests |
| Modify | `src/router/mod.rs` | Add `pub mod declarative_policy;` |
| Modify | `src/router/policy.rs` | Add `settings` optional field + `.with_settings()` builder; integrate evaluator |
| Modify | `src/cli/mod.rs` | Pass `live_settings` to `PolicyEngine` at construction |
| Modify | `config.example.toml` | Add documented `[[policy.rules]]` examples |

---

### Task 1: Config schema — PolicyRuleConfig

**Files:**
- Modify: `src/config/schema.rs`

- [ ] **Step 1: Write the failing test**

Add at the very bottom of `src/config/schema.rs`:

```rust
#[cfg(test)]
mod policy_rule_tests {
    use super::*;

    #[test]
    fn policy_rule_defaults() {
        let rule: PolicyRuleConfig = toml::from_str(r#"
            name = "test"
        "#).unwrap();
        assert_eq!(rule.name, "test");
        assert_eq!(rule.priority, 0);
        assert_eq!(rule.window, "monthly");
        assert!(rule.allow_models.is_empty());
        assert!(rule.budget_usd.is_none());
        assert!(rule.condition.tag.is_none());
    }

    #[test]
    fn policy_rule_full_parse() {
        let rule: PolicyRuleConfig = toml::from_str(r#"
            name = "research-team-opus"
            priority = 10
            allow_models = ["claude-opus-4-5"]
            budget_usd = 200.0
            window = "monthly"
            [condition]
            tag = "research"
        "#).unwrap();
        assert_eq!(rule.priority, 10);
        assert_eq!(rule.condition.tag.as_deref(), Some("research"));
        assert_eq!(rule.budget_usd, Some(200.0));
    }

    #[test]
    fn settings_policy_rules_field() {
        let s: Settings = toml::from_str(r#"
            [[policy_rules]]
            name = "allow-all"
        "#).unwrap();
        assert_eq!(s.policy_rules.len(), 1);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test policy_rule_defaults 2>&1 | head -20`
Expected: compile error — `PolicyRuleConfig` not defined

- [ ] **Step 3: Add the config structs to src/config/schema.rs**

Insert these structs just before the `#[cfg(test)]` block at the bottom (and before the existing closing brace of the file):

```rust
/// Condition for a declarative policy rule. All provided fields must match.
/// An empty condition matches every request.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PolicyConditionConfig {
    /// Match on the user's API key tag.
    pub tag: Option<String>,
    /// Match on the user's group name.
    pub group_name: Option<String>,
    /// Match on a specific user ID.
    pub user_id: Option<i64>,
    /// Match on the requested model string.
    pub model: Option<String>,
}

fn default_policy_window() -> String { "monthly".to_string() }

/// A single declarative policy rule.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolicyRuleConfig {
    /// Human-readable name (logged on match, not used for uniqueness checks).
    pub name: String,
    /// Conditions that must ALL match for this rule to apply.
    #[serde(default)]
    pub condition: PolicyConditionConfig,
    /// Allowlist of model strings. If non-empty, any model not in the list is denied 403.
    #[serde(default)]
    pub allow_models: Vec<String>,
    /// USD spend limit for the window. None = no budget cap.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    /// Budget window: "daily", "weekly", or "monthly".
    #[serde(default = "default_policy_window")]
    pub window: String,
    /// Sort order — higher priority rules are evaluated first. Default 0.
    #[serde(default)]
    pub priority: i32,
}
```

Also add `policy_rules` to the `Settings` struct. Find the existing `pub guardrails: Vec<GuardrailConfig>,` line and add after it:

```rust
    #[serde(default)]
    pub policy_rules: Vec<PolicyRuleConfig>,
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test policy_rule 2>&1`
Expected: 3 tests pass

- [ ] **Step 5: Commit**

```bash
git add src/config/schema.rs
git commit -m "feat: add PolicyRuleConfig and PolicyConditionConfig to settings schema"
```

---

### Task 2: DeclarativePolicyEvaluator module

**Files:**
- Create: `src/router/declarative_policy.rs`
- Modify: `src/router/mod.rs`

The evaluator is pure logic — no database access, no async. This makes it fully unit-testable.

- [ ] **Step 1: Write the failing tests first**

Create `src/router/declarative_policy.rs` containing only the test module for now:

```rust
// src/router/declarative_policy.rs

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{PolicyConditionConfig, PolicyRuleConfig};
    use crate::db::models::User;

    fn user(tag: Option<&str>, group: Option<&str>, id: i64) -> User {
        User {
            id,
            name: "test".to_string(),
            api_key: "hash".to_string(),
            api_key_old: None,
            api_key_old_expires_at: None,
            group_name: group.map(str::to_string),
            enabled: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            metadata: "{}".to_string(),
            api_key_id: None,
            spend_reset_at: None,
            api_key_tag: tag.map(str::to_string),
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
        let u = user(None, None, 1);
        let r = rule("open", 0, PolicyConditionConfig::default());
        assert!(condition_matches(&r.condition, &u, "gpt-4o"));
    }

    #[test]
    fn tag_condition_matches_exact() {
        let u = user(Some("research"), None, 1);
        let cond = PolicyConditionConfig { tag: Some("research".to_string()), ..Default::default() };
        assert!(condition_matches(&cond, &u, "gpt-4o"));
    }

    #[test]
    fn tag_condition_rejects_wrong_tag() {
        let u = user(Some("intern"), None, 1);
        let cond = PolicyConditionConfig { tag: Some("research".to_string()), ..Default::default() };
        assert!(!condition_matches(&cond, &u, "gpt-4o"));
    }

    #[test]
    fn tag_condition_rejects_no_tag() {
        let u = user(None, None, 1);
        let cond = PolicyConditionConfig { tag: Some("research".to_string()), ..Default::default() };
        assert!(!condition_matches(&cond, &u, "gpt-4o"));
    }

    #[test]
    fn group_condition_matches() {
        let u = user(None, Some("admins"), 1);
        let cond = PolicyConditionConfig { group_name: Some("admins".to_string()), ..Default::default() };
        assert!(condition_matches(&cond, &u, "gpt-4o"));
    }

    #[test]
    fn user_id_condition_matches() {
        let u = user(None, None, 42);
        let cond = PolicyConditionConfig { user_id: Some(42), ..Default::default() };
        assert!(condition_matches(&cond, &u, "gpt-4o"));
    }

    #[test]
    fn model_condition_matches() {
        let u = user(None, None, 1);
        let cond = PolicyConditionConfig { model: Some("claude-opus-4-5".to_string()), ..Default::default() };
        assert!(condition_matches(&cond, &u, "claude-opus-4-5"));
        assert!(!condition_matches(&cond, &u, "gpt-4o"));
    }

    #[test]
    fn multiple_conditions_all_must_match() {
        // tag=research AND group=ml
        let u_both = user(Some("research"), Some("ml"), 1);
        let u_tag_only = user(Some("research"), None, 1);
        let cond = PolicyConditionConfig {
            tag: Some("research".to_string()),
            group_name: Some("ml".to_string()),
            ..Default::default()
        };
        assert!(condition_matches(&cond, &u_both, "gpt-4o"));
        assert!(!condition_matches(&cond, &u_tag_only, "gpt-4o"));
    }

    #[test]
    fn matching_rule_picks_highest_priority() {
        let u = user(Some("research"), None, 1);
        let rules = vec![
            rule("low", 1, PolicyConditionConfig { tag: Some("research".to_string()), ..Default::default() }),
            rule("high", 10, PolicyConditionConfig { tag: Some("research".to_string()), ..Default::default() }),
        ];
        let found = find_matching_rule(&rules, &u, "gpt-4o");
        assert_eq!(found.map(|r| r.name.as_str()), Some("high"));
    }

    #[test]
    fn matching_rule_returns_none_when_no_match() {
        let u = user(None, None, 1);
        let rules = vec![
            rule("tag-only", 5, PolicyConditionConfig { tag: Some("research".to_string()), ..Default::default() }),
        ];
        assert!(find_matching_rule(&rules, &u, "gpt-4o").is_none());
    }

    #[test]
    fn matching_rule_returns_none_for_empty_rules() {
        let u = user(None, None, 1);
        assert!(find_matching_rule(&[], &u, "gpt-4o").is_none());
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test declarative_policy 2>&1 | head -20`
Expected: compile error — functions not defined

- [ ] **Step 3: Write the implementation (prepend before the test module)**

```rust
use crate::config::schema::{PolicyConditionConfig, PolicyRuleConfig};
use crate::db::models::User;

/// Returns true if ALL non-None fields in `condition` match the given user and model.
/// An all-None condition matches everything.
pub fn condition_matches(condition: &PolicyConditionConfig, user: &User, model: &str) -> bool {
    if let Some(tag) = &condition.tag {
        if user.api_key_tag.as_deref() != Some(tag.as_str()) {
            return false;
        }
    }
    if let Some(group) = &condition.group_name {
        if user.group_name.as_deref() != Some(group.as_str()) {
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
/// Rules with the same priority are evaluated in the order provided; the first matching
/// one wins (stable tie-breaking — callers should ensure unique priorities for determinism).
pub fn find_matching_rule<'a>(
    rules: &'a [PolicyRuleConfig],
    user: &User,
    model: &str,
) -> Option<&'a PolicyRuleConfig> {
    let mut sorted: Vec<&PolicyRuleConfig> = rules.iter().collect();
    sorted.sort_by(|a, b| b.priority.cmp(&a.priority));
    sorted.into_iter().find(|r| condition_matches(&r.condition, user, model))
}
```

- [ ] **Step 4: Register in src/router/mod.rs**

Add `pub mod declarative_policy;` alongside the other mod declarations.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test declarative_policy 2>&1 | tail -20`
Expected: 11 tests pass

- [ ] **Step 6: Commit**

```bash
git add src/router/declarative_policy.rs src/router/mod.rs
git commit -m "feat: DeclarativePolicyEvaluator — condition matching with full unit tests"
```

---

### Task 3: PolicyEngine integration

**Files:**
- Modify: `src/router/policy.rs`

Add an optional `settings` field to `PolicyEngine` using a builder method. The existing `new(db)` signature is kept intact so all test callers compile without changes. When settings are present, the engine runs declarative rules first; if a rule matches, it enforces the rule and returns early (bypassing DB rules). If no rule matches, the existing DB rule path runs unchanged.

- [ ] **Step 1: Run existing tests to confirm baseline**

Run: `cargo test 2>&1 | tail -5`
Expected: All pass (confirm green before modifying policy.rs)

- [ ] **Step 2: Add settings field and builder**

Read `src/router/policy.rs`. Add these imports at the top:

```rust
use arc_swap::ArcSwap;
use crate::config::schema::Settings;
use crate::router::declarative_policy::find_matching_rule;
```

Change the `PolicyEngine` struct:

```rust
pub struct PolicyEngine {
    pub db: Arc<dyn DatabaseProvider>,
    settings: Option<Arc<ArcSwap<Settings>>>,
}
```

Update `PolicyEngine::new` and add builder method:

```rust
impl PolicyEngine {
    pub fn new(db: Arc<dyn DatabaseProvider>) -> Self {
        Self { db, settings: None }
    }

    /// Attach live settings for declarative policy rule evaluation.
    pub fn with_settings(mut self, settings: Arc<ArcSwap<Settings>>) -> Self {
        self.settings = Some(settings);
        self
    }
    // ... keep existing check() method
```

- [ ] **Step 3: Add declarative rule evaluation at the start of check()**

At the very beginning of the `check()` method body, before any DB repository calls, insert:

```rust
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
```

- [ ] **Step 4: Run full test suite to verify no regressions**

Run: `cargo test 2>&1 | tail -20`
Expected: All tests still pass (existing tests use `PolicyEngine::new(db)` which leaves settings=None, so declarative block is skipped entirely)

- [ ] **Step 5: Commit**

```bash
git add src/router/policy.rs
git commit -m "feat: integrate declarative policy rules into PolicyEngine"
```

---

### Task 4: Wire settings into production PolicyEngine

**Files:**
- Modify: `src/cli/mod.rs`

The production `PolicyEngine` is constructed at `src/cli/mod.rs:232`. Update it to attach `live_settings`.

- [ ] **Step 1: Read the construction context**

Read `src/cli/mod.rs` around line 232. Find both:
- The line `let policy = Arc::new(crate::router::policy::PolicyEngine::new(db.clone()));`
- The line where `live_settings` (or the `Arc<ArcSwap<Settings>>`) is declared

Note which line number each is on.

- [ ] **Step 2: Ensure live_settings is in scope**

**IMPORTANT:** `live_settings` may be declared AFTER the `PolicyEngine` construction line. If so, move the entire `let policy = ...` statement to after the `live_settings` declaration. This is safe — `policy` is only used later in the `AppState` construction which is after both.

- [ ] **Step 3: Update the construction call**

Find:
```rust
let policy = Arc::new(crate::router::policy::PolicyEngine::new(db.clone()));
```

Replace with (using the correct variable name found in Step 1):
```rust
let policy = Arc::new(
    crate::router::policy::PolicyEngine::new(db.clone())
        .with_settings(live_settings.clone()),
);
```

- [ ] **Step 4: Verify compile**

Run: `cargo check 2>&1 | tail -5`
Expected: No errors

- [ ] **Step 5: Run full test suite**

Run: `cargo test 2>&1 | tail -10`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add src/cli/mod.rs
git commit -m "feat: wire live_settings into production PolicyEngine"
```

---

### Task 5: Integration tests for PolicyEngine with declarative rules

**Files:**
- Create: `tests/test_declarative_policy.rs`

These tests construct a `PolicyEngine` with settings attached and drive it end-to-end: model deny, budget deny, and fallthrough to DB rules when no config rule matches.

- [ ] **Step 1: Write the failing tests**

Create `tests/test_declarative_policy.rs`:

```rust
//! Integration tests for PolicyEngine with declarative (config-driven) rules.
//! Uses an in-memory SQLite DB and arc_swap::ArcSwap<Settings>.

use std::sync::Arc;
use arc_swap::ArcSwap;
use modelrouter::{
    config::schema::{Settings, PolicyRuleConfig, PolicyConditionConfig},
    db::{models::User, sqlite::SqliteDb},
    router::policy::{PolicyDecision, PolicyEngine},
};

async fn test_db() -> Arc<SqliteDb> {
    let db = SqliteDb::connect(":memory:").await.unwrap();
    sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
    Arc::new(db)
}

fn test_user() -> User {
    User {
        id: 1,
        name: "test".to_string(),
        api_key: "hash".to_string(),
        api_key_old: None,
        api_key_old_expires_at: None,
        group_name: Some("research".to_string()),
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        metadata: "{}".to_string(),
        api_key_id: None,
        spend_reset_at: None,
        api_key_tag: Some("research".to_string()),
    }
}

fn settings_with_rules(rules: Vec<PolicyRuleConfig>) -> Arc<ArcSwap<Settings>> {
    let mut s = Settings::default();
    s.policy_rules = rules;
    Arc::new(ArcSwap::from_pointee(s))
}

#[tokio::test]
async fn declarative_rule_denies_disallowed_model() {
    let db = test_db().await;
    let settings = settings_with_rules(vec![PolicyRuleConfig {
        name: "research-only-opus".to_string(),
        condition: PolicyConditionConfig {
            tag: Some("research".to_string()),
            ..Default::default()
        },
        allow_models: vec!["claude-opus-4-5".to_string()],
        budget_usd: None,
        window: "monthly".to_string(),
        priority: 10,
    }]);

    let engine = PolicyEngine::new(db).with_settings(settings);
    let decision = engine.check(&test_user(), "gpt-4o").await.unwrap();
    assert!(matches!(decision, PolicyDecision::Deny { status: 403, .. }));
}

#[tokio::test]
async fn declarative_rule_allows_permitted_model() {
    let db = test_db().await;
    let settings = settings_with_rules(vec![PolicyRuleConfig {
        name: "research-only-opus".to_string(),
        condition: PolicyConditionConfig {
            tag: Some("research".to_string()),
            ..Default::default()
        },
        allow_models: vec!["claude-opus-4-5".to_string()],
        budget_usd: None,
        window: "monthly".to_string(),
        priority: 10,
    }]);

    let engine = PolicyEngine::new(db).with_settings(settings);
    let decision = engine.check(&test_user(), "claude-opus-4-5").await.unwrap();
    assert!(matches!(decision, PolicyDecision::Allow { .. }));
}

#[tokio::test]
async fn declarative_rule_allows_under_budget() {
    let db = test_db().await;
    // Budget of $100.0 — fresh DB has zero spend, so under limit → Allow
    let settings = settings_with_rules(vec![PolicyRuleConfig {
        name: "generous-budget".to_string(),
        condition: PolicyConditionConfig::default(), // matches all
        allow_models: vec![],
        budget_usd: Some(100.0),
        window: "monthly".to_string(),
        priority: 5,
    }]);

    let engine = PolicyEngine::new(db).with_settings(settings);
    // 0.0 spent < 100.0 limit → Allow
    let decision = engine.check(&test_user(), "gpt-4o").await.unwrap();
    assert!(matches!(decision, PolicyDecision::Allow { .. }));
}

#[tokio::test]
async fn no_matching_rule_falls_through_to_db_rules() {
    let db = test_db().await;
    // Rule only matches tag="premium" — our user has tag="research"
    let settings = settings_with_rules(vec![PolicyRuleConfig {
        name: "premium-only".to_string(),
        condition: PolicyConditionConfig {
            tag: Some("premium".to_string()),
            ..Default::default()
        },
        allow_models: vec!["gpt-4o".to_string()],
        budget_usd: None,
        window: "monthly".to_string(),
        priority: 10,
    }]);

    let engine = PolicyEngine::new(db).with_settings(settings);
    // User has tag="research" — no config rule matches — falls through to DB rules
    // DB has no budget rules for user 1, so it should Allow
    let decision = engine.check(&test_user(), "gpt-4o").await.unwrap();
    assert!(matches!(decision, PolicyDecision::Allow { .. }));
}

#[tokio::test]
async fn empty_policy_rules_falls_through_to_db_rules() {
    let db = test_db().await;
    let settings = settings_with_rules(vec![]); // No rules
    let engine = PolicyEngine::new(db).with_settings(settings);
    let decision = engine.check(&test_user(), "gpt-4o").await.unwrap();
    assert!(matches!(decision, PolicyDecision::Allow { .. }));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test declarative_policy 2>&1 | head -20`
Expected: compile error (types not yet public enough, or test setup issues)

Fix any compile issues (e.g. making `PolicyEngine` and `PolicyDecision` pub in their modules — they already are), then re-run.

- [ ] **Step 3: Run to verify all 5 tests pass**

Run: `cargo test declarative_policy 2>&1 | tail -20`
Expected: 5 tests pass

- [ ] **Step 4: Commit**

```bash
git add tests/test_declarative_policy.rs
git commit -m "test: integration tests for PolicyEngine with declarative rules"
```

---

### Task 6: Document in config.example.toml

**Files:**
- Modify: `config.example.toml`

- [ ] **Step 1: Add the example policy rules section**

Open `config.example.toml` and append this section at the end:

```toml
# ---------------------------------------------------------------------------
# Declarative Policy Rules
# ---------------------------------------------------------------------------
# Rules are evaluated before database budget rules. The highest-priority
# matching rule wins. If no rule matches, database rules apply (backward compat).
#
# Condition fields (all optional — omit to match everyone):
#   tag        = user's API key tag
#   group_name = user's group
#   user_id    = specific user ID
#   model      = exact requested model string
#
# All condition fields provided must match (AND logic).
#
# [[policy_rules]]
# name         = "research-team-opus"
# priority     = 10
# allow_models = ["claude-opus-4-5"]
# budget_usd   = 200.0
# window       = "monthly"         # "daily", "weekly", or "monthly"
# [policy_rules.condition]
# tag = "research"
#
# [[policy_rules]]
# name     = "intern-budget"
# priority = 5
# budget_usd = 10.0
# window   = "monthly"
# [policy_rules.condition]
# group_name = "interns"
#
# [[policy_rules]]
# name     = "global-floor"
# priority = 0
# budget_usd = 50.0
# window   = "monthly"
# # No condition — matches all users (catch-all / default policy)
```

- [ ] **Step 2: Verify config still parses**

Run: `cargo test 2>&1 | tail -5`
Expected: All pass (no config parsing regressions)

- [ ] **Step 3: Commit**

```bash
git add config.example.toml
git commit -m "docs: add declarative policy_rules examples to config.example.toml"
```

---

### Task 7: Final verification

- [ ] **Step 1: Run full test suite**

Run: `cargo test 2>&1 | tail -30`
Expected: All tests pass

- [ ] **Step 2: Run release build**

Run: `cargo build --release 2>&1 | tail -5`
Expected: Compiles successfully

- [ ] **Step 3: End-to-end smoke test — verify a rule can be loaded**

Run:
```bash
cd /Users/Keith.MacKay/Projects/modelrouter
cargo test policy_rule_full_parse 2>&1
```
Expected: PASS — confirms the TOML round-trip works

- [ ] **Step 4: Push to main**

```bash
git push origin main
```
