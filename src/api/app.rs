use std::sync::Arc;

use crate::{
    config::Settings,
    db::repositories::{
        admin_users::AdminUserRepository, audit::AuditRepository, budgets::BudgetRepository,
        costs::CostRepository, hooks::HookRepository, prompts::PromptRepository,
        rate_limits::RateLimitRepository, sessions::SessionRepository, users::UserRepository,
    },
    providers::registry::ProviderRegistry,
    router::{cost::CostCalculator, engine::RequestRouter, policy::PolicyEngine},
};

/// Aggregated DB trait — SqliteDb implements this via blanket impl
pub trait DatabaseProvider:
    UserRepository
    + AdminUserRepository
    + SessionRepository
    + PromptRepository
    + CostRepository
    + BudgetRepository
    + AuditRepository
    + HookRepository
    + RateLimitRepository
    + Send
    + Sync
{
}

/// Blanket impl so any type implementing all sub-traits automatically impl DatabaseProvider
impl<T> DatabaseProvider for T where
    T: UserRepository
        + AdminUserRepository
        + SessionRepository
        + PromptRepository
        + CostRepository
        + BudgetRepository
        + AuditRepository
        + HookRepository
        + RateLimitRepository
        + Send
        + Sync
{
}

#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Settings>,
    pub db: Arc<dyn DatabaseProvider>,
    pub pool: Option<sqlx::SqlitePool>,
    pub router: Arc<RequestRouter>,
    pub cost_calc: Arc<CostCalculator>,
    pub provider_registry: Arc<ProviderRegistry>,
    pub policy: Arc<PolicyEngine>,
}

pub fn build_router(state: AppState) -> axum::Router {
    use axum::routing::{delete, get, patch, post};
    use crate::api::routes::{
        completions::chat_completions, health::health_check, models::list_models,
    };
    use crate::api::admin::routes::{
        admin_login, list_users, create_user, update_user, rotate_user_key,
        list_budgets, create_budget, delete_budget,
        get_stats, get_audit, get_prompts,
        list_admins, create_admin,
    };
    use crate::api::admin::dashboard::{
        get_login, post_login, post_logout,
        get_overview, get_users, post_disable_user, post_enable_user, post_rotate_user_key,
        get_prompts as dash_get_prompts, get_prompt_detail,
        get_cost, get_hooks,
        get_audit as dash_get_audit,
        get_admins, post_create_admin, post_delete_admin,
    };

    axum::Router::new()
        // Health + API routes
        .route("/health", get(health_check))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        // Admin REST API
        .route("/admin/api/login", post(admin_login))
        .route("/admin/api/users", get(list_users).post(create_user))
        .route("/admin/api/users/:id", patch(update_user))
        .route("/admin/api/users/:id/rotate-key", post(rotate_user_key))
        .route("/admin/api/budgets", get(list_budgets).post(create_budget))
        .route("/admin/api/budgets/:id", delete(delete_budget))
        .route("/admin/api/stats", get(get_stats))
        .route("/admin/api/audit", get(get_audit))
        .route("/admin/api/prompts", get(get_prompts))
        .route("/admin/api/admins", get(list_admins).post(create_admin))
        // Admin Dashboard (public)
        .route("/admin/login", get(get_login).post(post_login))
        .route("/admin/logout", post(post_logout))
        // Admin Dashboard (requires DashboardSession cookie)
        .route("/admin", get(get_overview))
        .route("/admin/users", get(get_users))
        .route("/admin/users/:id/disable", post(post_disable_user))
        .route("/admin/users/:id/enable", post(post_enable_user))
        .route("/admin/users/:id/rotate-key", post(post_rotate_user_key))
        .route("/admin/prompts", get(dash_get_prompts))
        .route("/admin/prompts/:id", get(get_prompt_detail))
        .route("/admin/cost", get(get_cost))
        .route("/admin/hooks", get(get_hooks))
        .route("/admin/audit", get(dash_get_audit))
        .route("/admin/admins", get(get_admins).post(post_create_admin))
        .route("/admin/admins/:id/delete", post(post_delete_admin))
        .with_state(state)
}
