use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;

use super::auth::{issue_jwt, verify_jwt, AdminClaims};
use super::audit::audit;
use crate::api::app::AppState;

// ── Template environment ──────────────────────────────────────────────────────

fn render(template: &str, ctx: minijinja::Value) -> Result<Html<String>, DashboardError> {
    let tmpl = super::templates::env()
        .get_template(template)
        .map_err(|e| DashboardError::Template(e.to_string()))?;
    let rendered = tmpl
        .render(ctx)
        .map_err(|e| DashboardError::Template(e.to_string()))?;
    Ok(Html(rendered))
}

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum DashboardError {
    Template(String),
    Unauthorized,
    Forbidden,
    BadRequest(String),
    Internal,
}

impl IntoResponse for DashboardError {
    fn into_response(self) -> Response {
        match self {
            DashboardError::Unauthorized => {
                Redirect::to("/admin/login").into_response()
            }
            DashboardError::Forbidden => {
                (StatusCode::FORBIDDEN, Html("<h1>403 Forbidden</h1>".to_string()))
                    .into_response()
            }
            DashboardError::Template(msg) => {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Html(format!("<h1>Template error</h1><pre>{}</pre>", msg)),
                )
                    .into_response()
            }
            DashboardError::BadRequest(msg) => {
                (StatusCode::BAD_REQUEST, Html(format!("<h1>Bad Request</h1><p>{}</p>", msg)))
                    .into_response()
            }
            DashboardError::Internal => {
                (StatusCode::INTERNAL_SERVER_ERROR, Html("<h1>Internal Error</h1>".to_string()))
                    .into_response()
            }
        }
    }
}

// ── DashboardSession extractor ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DashboardSession(pub AdminClaims);

#[async_trait::async_trait]
impl axum::extract::FromRequestParts<AppState> for DashboardSession {
    type Rejection = DashboardError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Extract mr_admin_session cookie
        let token = parts
            .headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|cookies| {
                cookies.split(';').find_map(|c| {
                    let c = c.trim();
                    c.strip_prefix("mr_admin_session=").map(|v| v.to_string())
                })
            })
            .ok_or(DashboardError::Unauthorized)?;

        let claims = verify_jwt(&token, &state.settings.auth.jwt_secret)
            .map_err(|_| DashboardError::Unauthorized)?;

        Ok(DashboardSession(claims))
    }
}

/// Superadmin dashboard guard
pub struct SuperDashboardSession(pub AdminClaims);

#[async_trait::async_trait]
impl axum::extract::FromRequestParts<AppState> for SuperDashboardSession {
    type Rejection = DashboardError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let session = DashboardSession::from_request_parts(parts, state).await?;
        if session.0.role != "superadmin" {
            return Err(DashboardError::Forbidden);
        }
        Ok(SuperDashboardSession(session.0))
    }
}

// ── Login / Logout ────────────────────────────────────────────────────────────

pub async fn get_login() -> Result<Html<String>, DashboardError> {
    render(
        "login.html",
        minijinja::context! { error => minijinja::Value::UNDEFINED },
    )
}

#[derive(Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
}

pub async fn post_login(
    State(state): State<AppState>,
    Form(body): Form<LoginForm>,
) -> Response {
    use crate::db::repositories::admin_users::AdminUserRepository;

    let result = async {
        let admin = AdminUserRepository::find_by_name(&*state.db, &body.username)
            .await
            .map_err(|_| "internal error")?
            .ok_or("invalid credentials")?;

        if !admin.enabled {
            return Err("account disabled");
        }

        let valid = bcrypt::verify(&body.password, &admin.password_hash)
            .map_err(|_| "internal error")?;
        if !valid {
            return Err("invalid credentials");
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
            .map_err(|_| "internal error")?;

        AdminUserRepository::update_last_login(&*state.db, admin.id)
            .await
            .ok();

        Ok(token)
    }
    .await;

    match result {
        Ok(token) => {
            // Set HttpOnly cookie and redirect to /admin
            // TODO: Add `; Secure` flag when TLS is configured (HTTPS deployments).
            // Currently omitted because this dev tool may run on plain HTTP.
            let cookie = format!(
                "mr_admin_session={}; Path=/; HttpOnly; SameSite=Lax",
                token
            );
            (
                StatusCode::SEE_OTHER,
                [
                    (header::LOCATION, "/admin".to_string()),
                    (header::SET_COOKIE, cookie),
                ],
            )
                .into_response()
        }
        Err(msg) => {
            // Re-render login with error
            match render(
                "login.html",
                minijinja::context! { error => msg },
            ) {
                Ok(html) => (StatusCode::OK, html).into_response(),
                Err(e) => e.into_response(),
            }
        }
    }
}

pub async fn post_logout() -> Response {
    // TODO: Add `; Secure` flag when TLS is configured (HTTPS deployments).
    let clear_cookie = "mr_admin_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0";
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/admin/login".to_string()),
            (header::SET_COOKIE, clear_cookie.to_string()),
        ],
    )
        .into_response()
}

// ── Overview ──────────────────────────────────────────────────────────────────

pub async fn get_overview(
    State(state): State<AppState>,
    _session: DashboardSession,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::{budgets::BudgetRepository, costs::CostRepository, users::UserRepository};

    // Get sqlite pool from db — we need to go through the trait
    // Use cost ledger sums via CostRepository
    let since_today = chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .to_rfc3339();

    let week_start = {
        use chrono::Datelike;
        let now = chrono::Utc::now();
        let days = now.weekday().num_days_from_monday() as i64;
        (now - chrono::Duration::days(days))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .to_rfc3339()
    };

    let month_start = {
        use chrono::Datelike;
        chrono::Utc::now()
            .with_day(1)
            .unwrap()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .to_rfc3339()
    };

    // Compute total spend across all users for each window
    let users = UserRepository::list(&*state.db)
        .await
        .map_err(|_| DashboardError::Internal)?;

    let mut spend_today = 0f64;
    let mut spend_week = 0f64;
    let mut spend_month = 0f64;
    let mut request_count = 0i64;

    for user in &users {
        spend_today += CostRepository::sum_for_user_since(&*state.db, user.id, &since_today)
            .await
            .unwrap_or(0.0);
        spend_week += CostRepository::sum_for_user_since(&*state.db, user.id, &week_start)
            .await
            .unwrap_or(0.0);
        spend_month += CostRepository::sum_for_user_since(&*state.db, user.id, &month_start)
            .await
            .unwrap_or(0.0);
    }

    // Total request count from prompts (using PromptRepository)
    use crate::db::repositories::prompts::PromptRepository;
    request_count = PromptRepository::count(&*state.db)
        .await
        .unwrap_or(0);

    // Budget warnings: users > 80% of monthly limit
    let mut budget_warnings: Vec<minijinja::Value> = Vec::new();
    for user in &users {
        let rules = BudgetRepository::list_for_user(&*state.db, user.id)
            .await
            .unwrap_or_default();
        for rule in &rules {
            if let Some(limit) = rule.limit_usd {
                let window_since = match rule.window.as_str() {
                    "daily" => since_today.clone(),
                    "weekly" => week_start.clone(),
                    _ => month_start.clone(),
                };
                let spend = CostRepository::sum_for_user_since(&*state.db, user.id, &window_since)
                    .await
                    .unwrap_or(0.0);
                if limit > 0.0 && spend / limit >= 0.8 {
                    budget_warnings.push(minijinja::context! {
                        user_name => user.name.clone(),
                        spend => spend,
                        limit => limit,
                        window => rule.window.clone(),
                    });
                }
            }
        }
    }

    render(
        "overview.html",
        minijinja::context! {
            spend_today => spend_today,
            spend_week => spend_week,
            spend_month => spend_month,
            request_count => request_count,
            budget_warnings => budget_warnings,
        },
    )
}

// ── User row fragment helper ──────────────────────────────────────────────────

fn user_row_html(id: i64, name: &str, group: &str, enabled: bool, created_at: &str, new_key: Option<&str>) -> String {
    let id_s = id.to_string();
    let status_tag = if enabled {
        "<span class=\"tag tag-enabled\">Enabled</span>"
    } else {
        "<span class=\"tag tag-disabled\">Disabled</span>"
    };

    // Build action buttons using string concatenation to avoid Rust 2021 format! issues
    let (toggle_action, toggle_label, toggle_class) = if enabled {
        ("/disable", "Disable", "btn btn-danger")
    } else {
        ("/enable", "Enable", "btn btn-success")
    };

    let toggle_btn = [
        "<button class=\"", toggle_class, "\" hx-post=\"/admin/users/",
        id_s.as_str(), toggle_action,
        "\" hx-target=\"#user-row-", id_s.as_str(),
        "\" hx-swap=\"outerHTML\">", toggle_label, "</button>",
    ].concat();

    let rotate_btn = [
        "<button class=\"btn btn-secondary\" hx-post=\"/admin/users/",
        id_s.as_str(), "/rotate-key",
        "\" hx-target=\"#user-row-", id_s.as_str(),
        "\" hx-swap=\"outerHTML\">Rotate Key</button>",
    ].concat();

    let new_key_html = if let Some(key) = new_key {
        ["<br><small style=\"color:#27ae60\">New key: ", key, "</small>"].concat()
    } else {
        String::new()
    };

    [
        "<tr id=\"user-row-", id_s.as_str(), "\">",
        "<td>", id_s.as_str(), "</td>",
        "<td>", name, "</td>",
        "<td>", group, "</td>",
        "<td>", status_tag, "</td>",
        "<td>", created_at, "</td>",
        "<td>",
        toggle_btn.as_str(), " ",
        rotate_btn.as_str(),
        new_key_html.as_str(),
        "</td></tr>",
    ].concat()
}

// ── Users page ────────────────────────────────────────────────────────────────

pub async fn get_users(
    State(state): State<AppState>,
    _session: DashboardSession,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::users::UserRepository;
    let users = UserRepository::list(&*state.db)
        .await
        .map_err(|_| DashboardError::Internal)?;

    let user_vals: Vec<minijinja::Value> = users
        .iter()
        .map(|u| {
            minijinja::context! {
                id => u.id,
                name => u.name.clone(),
                group_name => u.group_name.clone(),
                enabled => u.enabled,
                created_at => u.created_at.clone(),
            }
        })
        .collect();

    render("users.html", minijinja::context! { users => user_vals })
}

pub async fn post_disable_user(
    State(state): State<AppState>,
    _session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::users::UserRepository;
    UserRepository::set_enabled(&*state.db, id, false)
        .await
        .map_err(|_| DashboardError::Internal)?;

    let user = UserRepository::find_by_id(&*state.db, id)
        .await
        .ok()
        .flatten();
    // Return updated row fragment
    let (name, group_name, created_at) = if let Some(u) = user {
        (u.name, u.group_name, u.created_at)
    } else {
        (format!("user:{}", id), None, String::new())
    };

    let html = user_row_html(id, &name, group_name.as_deref().unwrap_or("—"), false, &created_at, None);
    Ok(Html(html))
}

pub async fn post_enable_user(
    State(state): State<AppState>,
    _session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::users::UserRepository;
    UserRepository::set_enabled(&*state.db, id, true)
        .await
        .map_err(|_| DashboardError::Internal)?;

    let user = UserRepository::find_by_id(&*state.db, id)
        .await
        .ok()
        .flatten();
    let (name, group_name, created_at) = if let Some(u) = user {
        (u.name, u.group_name, u.created_at)
    } else {
        (format!("user:{}", id), None, String::new())
    };

    let html = user_row_html(id, &name, group_name.as_deref().unwrap_or("—"), true, &created_at, None);
    Ok(Html(html))
}

pub async fn post_rotate_user_key(
    State(state): State<AppState>,
    session: DashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::users::UserRepository;
    use crate::db::repositories::api_keys::ApiKeyRepository;
    use crate::api::auth::hash_token;

    let new_token = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let new_hash = hash_token(&new_token);

    state.db.disable_all_keys_for_user(id)
        .await
        .map_err(|_| DashboardError::Internal)?;
    state.db.create_api_key(crate::db::models::NewApiKey {
        user_id: id,
        key_hash: new_hash,
        label: Some("dashboard-rotate".to_string()),
        expires_at: None,
        project: None,
    })
    .await
    .map_err(|_| DashboardError::Internal)?;

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

    let user = UserRepository::find_by_id(&*state.db, id)
        .await
        .ok()
        .flatten();
    let (name, group_name, created_at) = if let Some(u) = user {
        (u.name, u.group_name, u.created_at)
    } else {
        (format!("user:{}", id), None, String::new())
    };

    let html = user_row_html(id, &name, group_name.as_deref().unwrap_or("—"), true, &created_at, Some(&new_token));
    Ok(Html(html))
}

// ── Create user ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateUserForm {
    pub name: String,
    pub group_name: Option<String>,
}

pub async fn post_create_user(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Form(form): Form<CreateUserForm>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::models::NewUser;
    use crate::db::repositories::users::UserRepository;

    let name = form.name.trim().to_string();
    if name.is_empty() {
        return Err(DashboardError::BadRequest("name is required".into()));
    }

    let group_name = form.group_name.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);

    let user = UserRepository::create(
        &*state.db,
        NewUser { name: name.clone(), group_name, email: None },
    )
    .await
    .map_err(|_| DashboardError::Internal)?;

    // Generate an initial API key for this user
    use crate::api::auth::hash_token;
    use crate::db::repositories::api_keys::ApiKeyRepository;
    let token = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let hash = hash_token(&token);
    state.db.create_api_key(crate::db::models::NewApiKey {
        user_id: user.id,
        key_hash: hash,
        label: Some("initial".to_string()),
        expires_at: None,
        project: None,
    })
    .await
    .map_err(|_| DashboardError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "user.create",
        Some(format!("user:{}", user.id)),
        None,
        Some(serde_json::json!({ "name": user.name }).to_string()),
    )
    .await;

    let html = user_row_html(
        user.id,
        &user.name,
        user.group_name.as_deref().unwrap_or("—"),
        true,
        &user.created_at,
        Some(&token),
    );
    Ok(Html(html))
}

// ── Prompts page ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PageQuery {
    pub page: Option<u32>,
}

pub async fn get_prompts(
    State(state): State<AppState>,
    _session: DashboardSession,
    Query(q): Query<PageQuery>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::prompts::PromptRepository;

    let page = q.page.unwrap_or(1).max(1) as i64;
    let per_page: i64 = 50;
    let offset = (page - 1) * per_page;

    let prompts = PromptRepository::list(&*state.db, per_page, offset)
        .await
        .map_err(|_| DashboardError::Internal)?;

    let has_next = prompts.len() as i64 == per_page;

    let page_items: Vec<minijinja::Value> = prompts
        .into_iter()
        .map(|p| {
            minijinja::context! {
                id => p.id,
                user_id => p.user_id,
                request_model => p.request_model,
                routed_model => p.routed_model,
                cost_usd => p.cost_usd,
                prompt_tokens => p.prompt_tokens,
                completion_tokens => p.completion_tokens,
                created_at => p.created_at,
            }
        })
        .collect();

    render(
        "prompts.html",
        minijinja::context! {
            prompts => page_items,
            page => page,
            has_next => has_next,
        },
    )
}

pub async fn get_prompt_detail(
    State(state): State<AppState>,
    _session: DashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::prompts::PromptRepository;

    match PromptRepository::find_by_id(&*state.db, id)
        .await
        .map_err(|_| DashboardError::Internal)?
    {
        Some(p) => {
            let html = format!(
                r#"<div style="padding:0.75rem; background:#f9f9f9; border:1px solid #eee; border-radius:4px; margin-top:0.5rem;">
                    <strong>Messages:</strong><pre style="white-space:pre-wrap; font-size:0.8rem;">{}</pre>
                    <strong>Response:</strong><pre style="white-space:pre-wrap; font-size:0.8rem;">{}</pre>
                    <strong>Finish:</strong> {} | <strong>Latency:</strong> {}ms
                </div>"#,
                html_escape(&p.messages),
                html_escape(p.response.as_deref().unwrap_or("(none)")),
                html_escape(p.finish_reason.as_deref().unwrap_or("—")),
                p.latency_ms.unwrap_or(0),
            );
            Ok(Html(html))
        }
        None => Ok(Html(format!("<div>Prompt {} not found.</div>", id))),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── Cost page ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CostQuery {
    pub user: Option<String>,
    pub window: Option<String>,
}

pub async fn get_cost(
    State(state): State<AppState>,
    _session: DashboardSession,
    Query(q): Query<CostQuery>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::{costs::CostRepository, users::UserRepository};

    let window = q.window.as_deref().unwrap_or("monthly");
    let valid_windows = ["daily", "weekly", "monthly"];
    let window = if valid_windows.contains(&window) { window } else { "monthly" };

    let window_since = crate::report::window_start_str(window)
        .map_err(|_| DashboardError::Internal)?;

    let users = UserRepository::list(&*state.db)
        .await
        .map_err(|_| DashboardError::Internal)?;

    // Filter by user name if provided
    let filter_user = q.user.as_deref().unwrap_or("").to_string();

    let mut rows: Vec<minijinja::Value> = Vec::new();

    // Collect cost data per user, per model from cost ledger
    // We'll aggregate simply: for each user fetch sum and build rows
    // Use the report module for proper aggregation
    // We need the pool — go through a direct query approach using the trait methods
    // Since we only have sum_for_user_since, do a simple per-user approach
    for user in &users {
        if !filter_user.is_empty() && user.name != filter_user {
            continue;
        }
        let cost = CostRepository::sum_for_user_since(&*state.db, user.id, &window_since)
            .await
            .unwrap_or(0.0);
        if cost > 0.0 {
            rows.push(minijinja::context! {
                user_name => user.name.clone(),
                model => "all".to_string(),
                total_cost_usd => cost,
                total_tokens_in => 0i64,
                total_tokens_out => 0i64,
                request_count => 0i64,
            });
        }
    }

    render(
        "cost.html",
        minijinja::context! {
            rows => rows,
            window => window,
            filter_user => filter_user,
        },
    )
}

// ── Hooks page ────────────────────────────────────────────────────────────────

pub async fn get_hooks(
    State(state): State<AppState>,
    _session: DashboardSession,
) -> Result<Html<String>, DashboardError> {
    let hook_vals: Vec<minijinja::Value> = if let Some(pool) = &state.pool {
        let stats = crate::report::hook_latency_stats(pool)
            .await
            .unwrap_or_default();
        stats
            .iter()
            .map(|h| {
                minijinja::context! {
                    hook_name => h.hook_name.clone(),
                    invocation_count => h.invocation_count,
                    success_rate => h.success_rate,
                    avg_duration_ms => h.avg_duration_ms,
                    p50_duration_ms => h.p50_duration_ms,
                    p95_duration_ms => h.p95_duration_ms,
                    p99_duration_ms => h.p99_duration_ms,
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    render("hooks.html", minijinja::context! { hooks => hook_vals })
}

// ── Audit page ────────────────────────────────────────────────────────────────

pub async fn get_audit(
    State(state): State<AppState>,
    _session: DashboardSession,
    Query(q): Query<PageQuery>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::audit::AuditRepository;

    let page = q.page.unwrap_or(1).max(1) as i64;
    let per_page: i64 = 100;
    let offset = (page - 1) * per_page;

    let entries = AuditRepository::list(&*state.db, per_page, offset)
        .await
        .map_err(|_| DashboardError::Internal)?;

    let has_next = entries.len() as i64 == per_page;

    let page_entries: Vec<minijinja::Value> = entries
        .into_iter()
        .map(|e| {
            minijinja::context! {
                id => e.id,
                actor_name => e.actor_name,
                action => e.action,
                target => e.target,
                created_at => e.created_at,
            }
        })
        .collect();

    render(
        "audit.html",
        minijinja::context! {
            entries => page_entries,
            page => page,
            has_next => has_next,
        },
    )
}

// ── Admins page (superadmin only) ─────────────────────────────────────────────

pub async fn get_admins(
    State(state): State<AppState>,
    session: SuperDashboardSession,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::admin_users::AdminUserRepository;

    let admins = AdminUserRepository::list(&*state.db)
        .await
        .map_err(|_| DashboardError::Internal)?;

    let admin_vals: Vec<minijinja::Value> = admins
        .iter()
        .map(|a| {
            minijinja::context! {
                id => a.id,
                name => a.name.clone(),
                role => a.role.clone(),
                enabled => a.enabled,
                last_login_at => a.last_login_at.clone(),
            }
        })
        .collect();

    render(
        "admins.html",
        minijinja::context! {
            admins => admin_vals,
            current_admin_id => session.0.sub,
        },
    )
}

#[derive(Deserialize)]
pub struct CreateAdminForm {
    pub name: String,
    pub password: String,
    pub role: Option<String>,
}

pub async fn post_create_admin(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Form(body): Form<CreateAdminForm>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::{models::NewAdminUser, repositories::admin_users::AdminUserRepository};

    let role = body.role.clone().unwrap_or_else(|| "viewer".to_string());
    let password_hash = bcrypt::hash(&body.password, bcrypt::DEFAULT_COST)
        .map_err(|_| DashboardError::Internal)?;

    let admin = AdminUserRepository::create(
        &*state.db,
        NewAdminUser {
            name: body.name.clone(),
            password_hash,
            role,
        },
    )
    .await
    .map_err(|_| DashboardError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "admin.create",
        Some(format!("admin:{}", admin.id)),
        None,
        Some(serde_json::json!({"id": admin.id, "name": admin.name}).to_string()),
    )
    .await;

    Ok(Html(format!(
        r#"<div class="alert" style="background:#d4edda; border:1px solid #28a745; color:#155724; padding:0.75rem; border-radius:4px;">
            Admin <strong>{}</strong> created successfully (role: {}).
        </div>"#,
        admin.name, admin.role,
    )))
}

pub async fn post_delete_admin(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::admin_users::AdminUserRepository;

    // Cannot delete self
    if id == session.0.sub {
        return Ok(Html("<td colspan='6'>Cannot delete yourself.</td>".to_string()));
    }

    AdminUserRepository::delete(&*state.db, id)
        .await
        .map_err(|_| DashboardError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "admin.delete",
        Some(format!("admin:{}", id)),
        None,
        None,
    )
    .await;

    // Return empty row (deleted)
    let id_s = id.to_string();
    let html = [
        "<tr id=\"admin-row-", id_s.as_str(),
        "\" style=\"opacity:0.4\"><td colspan=\"6\"><em>Deleted</em></td></tr>",
    ].concat();
    Ok(Html(html))
}
