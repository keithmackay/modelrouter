use std::sync::Arc;

use arc_swap::ArcSwap;
use tower_http::trace::TraceLayer;

use crate::{
    config::Settings,
    db::repositories::{
        admin_users::AdminUserRepository, api_keys::ApiKeyRepository, audit::AuditRepository,
        budgets::BudgetRepository, costs::CostRepository, hooks::HookRepository,
        prompts::PromptRepository, rate_limits::RateLimitRepository, sessions::SessionRepository,
        users::UserRepository,
    },
    providers::{embed_registry::EmbeddingRegistry, registry::ProviderRegistry},
    router::{cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine},
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
    + ApiKeyRepository
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
        + ApiKeyRepository
        + Send
        + Sync
{
}

#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Settings>,
    pub live_settings: Arc<ArcSwap<Settings>>,
    pub db: Arc<dyn DatabaseProvider>,
    pub pool: Option<sqlx::SqlitePool>,
    pub router: Arc<RequestRouter>,
    pub cost_calc: Arc<CostCalculator>,
    pub provider_registry: Arc<ProviderRegistry>,
    pub policy: Arc<PolicyEngine>,
    pub fallback: Arc<FallbackChain>,
    pub complexity_router: Arc<crate::router::complexity::ComplexityRouter>,
    pub response_cache: Arc<crate::router::cache::ResponseCache>,
    pub embedding_registry: Arc<EmbeddingRegistry>,
    pub load_balancer: Arc<crate::router::load_balancer::LoadBalancer>,
    pub concurrency: Arc<crate::router::concurrency::ConcurrencyLimiter>,
    pub circuit_breaker: Arc<crate::router::circuit_breaker::CircuitBreaker>,
    pub ip_rate_limiter: Arc<crate::api::middleware::ip_rate_limit::IpRateLimiter>,
    pub session_limiter: Arc<crate::router::session_limits::SessionLimiter>,
    pub callbacks: Arc<crate::callbacks::CallbackDispatcher>,
    #[cfg(feature = "prometheus")]
    pub app_metrics: Option<Arc<crate::metrics::AppMetrics>>,
    #[cfg(not(feature = "prometheus"))]
    pub app_metrics: Option<std::convert::Infallible>,
}

pub fn build_router(state: AppState) -> axum::Router {
    use axum::routing::{delete, get, patch, post};
    use crate::api::routes::{
        audio::{speech, transcriptions},
        completions::chat_completions, embeddings::embeddings, health::health_check,
        images::image_generations, messages::anthropic_messages, models::list_models,
        prometheus::metrics_handler, responses::responses_handler,
    };
    use crate::api::admin::routes::{
        admin_login, list_users, create_user, update_user, rotate_user_key,
        list_budgets, create_budget, delete_budget,
        get_stats, get_audit, get_prompts,
        list_admins, create_admin,
        list_user_api_keys, create_user_api_key, revoke_api_key_handler,
        reset_user_spend,
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
        .route("/metrics", get(metrics_handler))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/responses", post(responses_handler))
        .route("/v1/images/generations", post(image_generations))
        .route("/v1/audio/speech", post(speech))
        .route("/v1/audio/transcriptions", post(transcriptions))
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
        .route("/admin/api/users/:id/keys", get(list_user_api_keys).post(create_user_api_key))
        .route("/admin/api/keys/:id/revoke", post(revoke_api_key_handler))
        .route("/admin/api/users/:id/reset-spend", post(reset_user_spend))
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
        .with_state(state.clone())
        .layer(TraceLayer::new_for_http())
        .layer(axum::middleware::from_fn_with_state(
            state.ip_rate_limiter.clone(),
            crate::api::middleware::ip_rate_limit::ip_rate_limit_middleware,
        ))
}
