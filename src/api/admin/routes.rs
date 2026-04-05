use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use crate::api::{app::AppState, error::ApiError};
use super::auth::{AdminSession, AdminClaims, SuperAdminSession, issue_jwt};
use super::audit::audit;

// ── Safe response types (no key hashes) ───────────────────────────────────────

#[derive(serde::Serialize)]
pub struct ApiKeyResponse {
    pub id: i64,
    pub user_id: i64,
    pub label: Option<String>,
    pub enabled: bool,
    pub created_at: String,
}

impl From<crate::db::models::ApiKey> for ApiKeyResponse {
    fn from(k: crate::db::models::ApiKey) -> Self {
        Self {
            id: k.id,
            user_id: k.user_id,
            label: k.label,
            enabled: k.enabled,
            created_at: k.created_at,
        }
    }
}

#[derive(serde::Serialize)]
struct UserResponse {
    id: i64,
    name: String,
    group_name: Option<String>,
    enabled: bool,
    created_at: String,
    metadata: String,
}

impl From<crate::db::models::User> for UserResponse {
    fn from(u: crate::db::models::User) -> Self {
        UserResponse {
            id: u.id,
            name: u.name,
            group_name: u.group_name,
            enabled: u.enabled,
            created_at: u.created_at,
            metadata: u.metadata,
        }
    }
}

// ── Login ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LoginRequest {
    pub name: String,
    pub password: String,
}

pub async fn admin_login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::admin_users::AdminUserRepository;

    let admin = AdminUserRepository::find_by_name(&*state.db, &body.name)
        .await
        .map_err(|_| ApiError::Internal)?
        .ok_or(ApiError::Unauthorized)?;

    if !admin.enabled {
        return Err(ApiError::Unauthorized);
    }

    let valid = bcrypt::verify(&body.password, &admin.password_hash)
        .map_err(|_| ApiError::Internal)?;
    if !valid {
        return Err(ApiError::Unauthorized);
    }

    let exp = (chrono::Utc::now()
        + chrono::Duration::minutes(state.settings.auth.jwt_expiry_mins))
    .timestamp() as usize;
    let claims = AdminClaims {
        sub: admin.id,
        name: admin.name.clone(),
        role: admin.role.clone(),
        exp,
    };
    let token = issue_jwt(&claims, &state.settings.auth.jwt_secret)
        .map_err(|_| ApiError::Internal)?;

    // Update last_login_at (fire-and-forget)
    AdminUserRepository::update_last_login(&*state.db, admin.id)
        .await
        .ok();

    Ok(Json(serde_json::json!({ "token": token })))
}

// ── User management ───────────────────────────────────────────────────────────

pub async fn list_users(
    State(state): State<AppState>,
    _session: AdminSession,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::users::UserRepository;
    let users = UserRepository::list(&*state.db)
        .await
        .map_err(|_| ApiError::Internal)?;
    let safe: Vec<UserResponse> = users.into_iter().map(UserResponse::from).collect();
    Ok(Json(safe))
}

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub name: String,
    pub group: Option<String>,
}

pub async fn create_user(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Json(body): Json<CreateUserRequest>,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::{models::NewUser, repositories::users::UserRepository};
    use crate::api::auth::hash_token;

    let raw_token = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let hash = hash_token(&raw_token);
    let user = UserRepository::create(
        &*state.db,
        NewUser {
            name: body.name.clone(),
            api_key_hash: hash,
            group_name: body.group.clone(),
        },
    )
    .await
    .map_err(|_| ApiError::Internal)?;

    let safe_user = UserResponse::from(user);
    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "user.create",
        Some(format!("user:{}", safe_user.id)),
        None,
        Some(serde_json::to_string(&safe_user).unwrap_or_default()),
    )
    .await;

    Ok(Json(serde_json::json!({
        "user": safe_user,
        "api_key": raw_token,
    })))
}

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    pub enabled: Option<bool>,
}

pub async fn update_user(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Path(id): Path<i64>,
    Json(body): Json<UpdateUserRequest>,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::users::UserRepository;

    if let Some(enabled) = body.enabled {
        UserRepository::set_enabled(&*state.db, id, enabled)
            .await
            .map_err(|_| ApiError::Internal)?;

        let action = if enabled { "user.enable" } else { "user.disable" };
        audit(
            &state.db,
            Some(session.0.sub),
            &session.0.name,
            action,
            Some(format!("user:{}", id)),
            None,
            None,
        )
        .await;
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn rotate_user_key(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::users::UserRepository;
    use crate::api::auth::hash_token;

    let new_token = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let new_hash = hash_token(&new_token);
    let overlap_expires_at = (chrono::Utc::now()
        + chrono::Duration::minutes(state.settings.auth.rotation_overlap_mins))
    .to_rfc3339();

    UserRepository::rotate_key(&*state.db, id, &new_hash, &overlap_expires_at)
        .await
        .map_err(|_| ApiError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "user.rotate_key",
        Some(format!("user:{}", id)),
        None,
        None,
    )
    .await;

    Ok(Json(serde_json::json!({
        "api_key": new_token,
        "old_key_valid_until": overlap_expires_at,
    })))
}

// ── Budget management ─────────────────────────────────────────────────────────

pub async fn list_budgets(
    State(state): State<AppState>,
    _session: AdminSession,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::budgets::BudgetRepository;
    let budgets = BudgetRepository::list_all(&*state.db)
        .await
        .map_err(|_| ApiError::Internal)?;
    Ok(Json(budgets))
}

#[derive(Deserialize)]
pub struct CreateBudgetRequest {
    pub user_id: Option<i64>,
    pub group_name: Option<String>,
    pub api_key_id: Option<i64>,
    pub window: String,
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<i64>,
    pub rate_rpm: Option<i64>,
    #[serde(default)]
    pub model_allow: Vec<String>,
    #[serde(default)]
    pub model_deny: Vec<String>,
}

pub async fn create_budget(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Json(body): Json<CreateBudgetRequest>,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::{models::NewBudgetRule, repositories::budgets::BudgetRepository};

    if !["daily", "weekly", "monthly"].contains(&body.window.as_str()) {
        return Err(ApiError::InvalidRequest(
            "window must be 'daily', 'weekly', or 'monthly'".to_string(),
        ));
    }

    let rule = BudgetRepository::create(
        &*state.db,
        NewBudgetRule {
            user_id: body.user_id,
            group_name: body.group_name,
            api_key_id: body.api_key_id,
            window: body.window,
            limit_usd: body.limit_usd,
            limit_tokens: body.limit_tokens,
            rate_rpm: body.rate_rpm,
            max_concurrent: None,
            model_allow: body.model_allow,
            model_deny: body.model_deny,
        },
    )
    .await
    .map_err(|_| ApiError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "budget.create",
        Some(format!("budget:{}", rule.id)),
        None,
        Some(serde_json::to_string(&rule).unwrap_or_default()),
    )
    .await;

    Ok(Json(rule))
}

pub async fn delete_budget(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::budgets::BudgetRepository;

    BudgetRepository::delete(&*state.db, id)
        .await
        .map_err(|_| ApiError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "budget.delete",
        Some(format!("budget:{}", id)),
        None,
        None,
    )
    .await;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Stats ─────────────────────────────────────────────────────────────────────

pub async fn get_stats(
    State(state): State<AppState>,
    _session: AdminSession,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::users::UserRepository;

    let users = UserRepository::list(&*state.db)
        .await
        .map_err(|_| ApiError::Internal)?;

    // Aggregate cost for each user for all-time (since epoch)
    let since = "1970-01-01T00:00:00+00:00";
    let mut stats = Vec::new();
    for user in &users {
        let total_cost = crate::db::repositories::costs::CostRepository::sum_for_user_since(
            &*state.db,
            user.id,
            since,
        )
        .await
        .unwrap_or(0.0);
        stats.push(serde_json::json!({
            "user_id": user.id,
            "name": user.name,
            "total_cost_usd": total_cost,
        }));
    }

    Ok(Json(stats))
}

// ── Audit ─────────────────────────────────────────────────────────────────────

pub async fn get_audit(
    State(state): State<AppState>,
    _session: AdminSession,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::audit::AuditRepository;
    let entries = AuditRepository::list(&*state.db, 100, 0)
        .await
        .map_err(|_| ApiError::Internal)?;
    Ok(Json(entries))
}

// ── Prompts ───────────────────────────────────────────────────────────────────

pub async fn get_prompts(
    State(state): State<AppState>,
    _session: AdminSession,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::{prompts::PromptRepository, users::UserRepository};

    let users = UserRepository::list(&*state.db)
        .await
        .map_err(|_| ApiError::Internal)?;

    let mut all_prompts = Vec::new();
    for user in &users {
        let prompts = PromptRepository::list_by_user(&*state.db, user.id, 20)
            .await
            .unwrap_or_default();
        all_prompts.extend(prompts);
    }

    Ok(Json(all_prompts))
}

// ── Admin user management ─────────────────────────────────────────────────────

pub async fn list_admins(
    State(state): State<AppState>,
    _session: SuperAdminSession,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::repositories::admin_users::AdminUserRepository;
    let admins = AdminUserRepository::list(&*state.db)
        .await
        .map_err(|_| ApiError::Internal)?;
    // Redact password hashes
    let safe: Vec<serde_json::Value> = admins
        .iter()
        .map(|a| {
            serde_json::json!({
                "id": a.id,
                "name": a.name,
                "role": a.role,
                "enabled": a.enabled,
                "created_at": a.created_at,
                "last_login_at": a.last_login_at,
            })
        })
        .collect();
    Ok(Json(safe))
}

#[derive(Deserialize)]
pub struct CreateAdminRequest {
    pub name: String,
    pub password: String,
    pub role: Option<String>,
}

pub async fn create_admin(
    State(state): State<AppState>,
    session: SuperAdminSession,
    Json(body): Json<CreateAdminRequest>,
) -> Result<impl IntoResponse, ApiError> {
    use crate::db::{models::NewAdminUser, repositories::admin_users::AdminUserRepository};

    let role = body.role.clone().unwrap_or_else(|| "viewer".to_string());
    if !["superadmin", "viewer"].contains(&role.as_str()) {
        return Err(ApiError::InvalidRequest(
            "role must be 'superadmin' or 'viewer'".to_string(),
        ));
    }

    let password_hash = bcrypt::hash(&body.password, bcrypt::DEFAULT_COST)
        .map_err(|_| ApiError::Internal)?;

    let admin = AdminUserRepository::create(
        &*state.db,
        NewAdminUser {
            name: body.name.clone(),
            password_hash,
            role,
        },
    )
    .await
    .map_err(|_| ApiError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "admin.create",
        Some(format!("admin:{}", admin.id)),
        None,
        Some(serde_json::json!({
            "id": admin.id,
            "name": admin.name,
            "role": admin.role,
        }).to_string()),
    )
    .await;

    Ok(Json(serde_json::json!({
        "id": admin.id,
        "name": admin.name,
        "role": admin.role,
        "created_at": admin.created_at,
    })))
}

// ── API key management ────────────────────────────────────────────────────────

// GET /admin/api/users/:id/keys — list API keys for user
pub async fn list_user_api_keys(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(user_id): Path<i64>,
) -> Result<Json<Vec<ApiKeyResponse>>, ApiError> {
    use crate::db::repositories::api_keys::ApiKeyRepository;
    let keys = ApiKeyRepository::list_api_keys_for_user(&*state.db, user_id)
        .await
        .map_err(|_| ApiError::Internal)?;
    Ok(Json(keys.into_iter().map(ApiKeyResponse::from).collect()))
}

// POST /admin/api/users/:id/keys — create API key for user
#[derive(serde::Deserialize)]
pub struct CreateApiKeyRequest {
    pub label: Option<String>,
}

pub async fn create_user_api_key(
    State(state): State<AppState>,
    admin: SuperAdminSession,
    Path(user_id): Path<i64>,
    Json(body): Json<CreateApiKeyRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use crate::db::repositories::api_keys::ApiKeyRepository;
    use crate::api::auth::hash_token;

    let raw_key = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let key_hash = hash_token(&raw_key);

    let created = ApiKeyRepository::create_api_key(&*state.db, crate::db::models::NewApiKey {
        user_id,
        key_hash,
        label: body.label,
        expires_at: None,
    })
    .await
    .map_err(|_| ApiError::Internal)?;

    audit(
        &state.db,
        Some(admin.0.sub),
        &admin.0.name,
        "create_api_key",
        Some(format!("user:{}", user_id)),
        None,
        Some(format!("label:{:?}", created.label)),
    )
    .await;

    // Return the raw key once — it cannot be recovered later
    Ok(Json(serde_json::json!({
        "id": created.id,
        "key": raw_key,
        "label": created.label,
        "created_at": created.created_at,
    })))
}

/// POST /admin/api/users/:id/reset-spend
pub async fn reset_user_spend(
    State(state): State<AppState>,
    _session: SuperAdminSession,
    Path(user_id): Path<i64>,
) -> Result<axum::Json<serde_json::Value>, ApiError> {
    use crate::db::repositories::users::UserRepository;
    state.db.reset_spend(user_id).await.map_err(|_| ApiError::Internal)?;
    Ok(axum::Json(serde_json::json!({ "user_id": user_id, "reset": true })))
}

// POST /admin/api/keys/:id/revoke — revoke API key
pub async fn revoke_api_key_handler(
    State(state): State<AppState>,
    admin: SuperAdminSession,
    Path(key_id): Path<i64>,
) -> Result<axum::http::StatusCode, ApiError> {
    use crate::db::repositories::api_keys::ApiKeyRepository;
    ApiKeyRepository::revoke_api_key(&*state.db, key_id)
        .await
        .map_err(|_| ApiError::Internal)?;

    audit(
        &state.db,
        Some(admin.0.sub),
        &admin.0.name,
        "revoke_api_key",
        Some(format!("key:{}", key_id)),
        None,
        None,
    )
    .await;

    Ok(axum::http::StatusCode::NO_CONTENT)
}
