use std::sync::Arc;

use crate::{
    config::Settings,
    db::repositories::{
        admin_users::AdminUserRepository, audit::AuditRepository, budgets::BudgetRepository,
        costs::CostRepository, hooks::HookRepository, prompts::PromptRepository,
        sessions::SessionRepository, users::UserRepository,
    },
    providers::registry::ProviderRegistry,
    router::{cost::CostCalculator, engine::RequestRouter},
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
        + Send
        + Sync
{
}

#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Settings>,
    pub db: Arc<dyn DatabaseProvider>,
    pub router: Arc<RequestRouter>,
    pub cost_calc: Arc<CostCalculator>,
    pub provider_registry: Arc<ProviderRegistry>,
}

pub fn build_router(state: AppState) -> axum::Router {
    use axum::routing::{get, post};
    use crate::api::routes::{
        completions::chat_completions, health::health_check, models::list_models,
    };

    axum::Router::new()
        .route("/health", get(health_check))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state)
}
