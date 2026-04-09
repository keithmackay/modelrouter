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
    NotFound(String),
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
            DashboardError::NotFound(msg) => {
                (StatusCode::NOT_FOUND, Html(format!("<h1>Not Found</h1><p>{}</p>", msg)))
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

fn user_row_html(user: &crate::db::models::User) -> String {
    let id_s = user.id.to_string();
    let status_tag = if user.enabled {
        "<span class=\"tag tag-enabled\">Enabled</span>"
    } else {
        "<span class=\"tag tag-disabled\">Disabled</span>"
    };

    let (toggle_action, toggle_label, toggle_class) = if user.enabled {
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

    let email_str = user.email.as_deref().unwrap_or("—");

    [
        "<tr id=\"user-row-", id_s.as_str(), "\">",
        "<td>", id_s.as_str(), "</td>",
        "<td>", user.name.as_str(), "</td>",
        "<td>", email_str, "</td>",
        "<td>", status_tag, "</td>",
        "<td>", user.created_at.as_str(), "</td>",
        "<td>", toggle_btn.as_str(), "</td>",
        "</tr>",
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
                email => u.email.clone(),
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
    session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::users::UserRepository;
    UserRepository::set_enabled(&*state.db, id, false)
        .await
        .map_err(|_| DashboardError::Internal)?;

    state.db.disable_all_keys_for_user(id)
        .await
        .map_err(|_| DashboardError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "user.disable",
        Some(format!("user:{}", id)),
        None,
        Some(serde_json::json!({ "enabled": false }).to_string()),
    )
    .await;

    let user = UserRepository::find_by_id(&*state.db, id)
        .await
        .map_err(|_| DashboardError::Internal)?
        .ok_or_else(|| DashboardError::NotFound(format!("user {} not found", id)))?;

    Ok(Html(user_row_html(&user)))
}

pub async fn post_enable_user(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::users::UserRepository;
    UserRepository::set_enabled(&*state.db, id, true)
        .await
        .map_err(|_| DashboardError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "user.enable",
        Some(format!("user:{}", id)),
        None,
        Some(serde_json::json!({ "enabled": true }).to_string()),
    )
    .await;

    let user = UserRepository::find_by_id(&*state.db, id)
        .await
        .map_err(|_| DashboardError::Internal)?
        .ok_or_else(|| DashboardError::NotFound(format!("user {} not found", id)))?;

    Ok(Html(user_row_html(&user)))
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

    let html = user_row_html(&user);
    Ok(Html(html))
}

// ── Keys page ─────────────────────────────────────────────────────────────────

struct KeyView {
    id: i64,
    user_id: i64,
    user_name: String,
    project: Option<String>,
    label: Option<String>,
    enabled: bool,
    created_at: String,
    disabled_at: Option<String>,
    raw_key: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct CreateKeyForm {
    pub user_name: String,
    pub project: String,
    pub label: String,
    pub email: Option<String>,
}

fn he(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Render a group of keys (1 active + N disabled) as a `<tbody>`.
/// group_id = id of the first/active key (used as the tbody anchor for hx-target).
fn key_group_html(group_id: i64, active: Option<&KeyView>, disabled_keys: &[KeyView], expanded: bool) -> String {
    let gid = group_id.to_string();
    // Sub-rows are all disabled keys when there's an active header, or all-but-first when all disabled
    let sub_keys = if active.is_some() { disabled_keys } else { &disabled_keys[1..] };
    let sub_count = sub_keys.len();

    let toggle_html = if sub_count > 0 {
        let arrow = if expanded { "▼" } else { "▶" };
        let label = if sub_count == 1 { "1 old key".to_string() } else { format!("{} old keys", sub_count) };
        format!(
            " <button id=\"toggle-{gid}\" type=\"button\" onclick=\"toggleGroup('{gid}')\" \
            class=\"btn btn-secondary\" data-count=\"{sub_count}\">{arrow} {label}</button>"
        )
    } else {
        String::new()
    };

    let header = match active {
        Some(k) => key_header_row_html(k, &gid, true, &toggle_html),
        None => match disabled_keys.first() {
            Some(k) => key_header_row_html(k, &gid, false, &toggle_html),
            None => return String::new(), // empty group — should not happen
        },
    };

    let display = if expanded { "table-row" } else { "none" };
    let sub_rows: String = sub_keys.iter().map(|k| key_sub_row_html(k, &gid, display)).collect();

    format!("<tbody id=\"key-group-{gid}\">{header}{sub_rows}</tbody>")
}

fn key_header_row_html(view: &KeyView, group_id: &str, is_active: bool, toggle_html: &str) -> String {
    let id_s = view.id.to_string();
    let status_tag = if view.enabled {
        "<span class=\"tag tag-enabled\">Enabled</span>"
    } else {
        "<span class=\"tag tag-disabled\">Disabled</span>"
    };

    let disable_btn = if is_active {
        format!(
            "<button class=\"btn btn-danger\" hx-post=\"/admin/keys/{id_s}/disable\" \
            hx-target=\"#key-group-{group_id}\" hx-swap=\"outerHTML\">Disable</button> "
        )
    } else {
        String::new()
    };

    let rotate_btn = format!(
        "<button class=\"btn btn-secondary\" hx-post=\"/admin/keys/{id_s}/rotate\" \
        hx-target=\"#key-group-{group_id}\" hx-swap=\"outerHTML\">Rotate</button>"
    );

    let raw_key_html = if let Some(raw) = &view.raw_key {
        let raw_e = he(raw);
        format!(
            "<br><span style=\"display:inline-flex;align-items:center;gap:0.4rem;margin-top:0.4rem;\">\
            <code style=\"color:green;font-family:monospace;font-size:0.85rem;background:#f0fff0;\
            padding:0.2rem 0.4rem;border-radius:3px;border:1px solid #c3e6cb;\">{raw_e}</code>\
            <button type=\"button\" title=\"Copy to clipboard\" \
            onclick=\"navigator.clipboard.writeText('{raw_e}').then(function(){{var b=this;\
            b.textContent='✓';setTimeout(function(){{b.textContent='⧉'}},1500)}}.bind(this))\" \
            style=\"background:none;border:1px solid #aaa;border-radius:3px;padding:0.1rem 0.35rem;\
            cursor:pointer;font-size:0.85rem;color:#555;\">⧉</button></span>"
        )
    } else {
        String::new()
    };

    let disabled_at = view.disabled_at.as_deref().unwrap_or("—");

    format!(
        "<tr id=\"key-row-{id_s}\">\
        <td>{user}</td><td>{proj}</td><td>{label}</td>\
        <td>{status_tag}</td><td>{created}</td><td>{disabled_at}</td>\
        <td>{disable_btn}{rotate_btn}{toggle_html}{raw_key_html}</td></tr>",
        user = he(&view.user_name),
        proj = he(view.project.as_deref().unwrap_or("—")),
        label = he(view.label.as_deref().unwrap_or("—")),
        created = view.created_at,
    )
}

fn key_sub_row_html(view: &KeyView, group_id: &str, display: &str) -> String {
    let id_s = view.id.to_string();
    let disabled_at = view.disabled_at.as_deref().unwrap_or("—");
    let rotate_btn = format!(
        "<button class=\"btn btn-secondary\" \
        hx-post=\"/admin/keys/{id_s}/rotate\" \
        hx-target=\"#key-group-{group_id}\" hx-swap=\"outerHTML\">Rotate</button>"
    );
    format!(
        "<tr id=\"key-row-{id_s}\" class=\"key-sub-row sub-{group_id}\" \
        style=\"display:{display};background:#fff0f0;\">\
        <td style=\"padding-left:2rem;\">↳ {user}</td>\
        <td>{proj}</td><td>{label}</td>\
        <td><span class=\"tag tag-disabled\">Disabled</span></td>\
        <td>{created}</td><td>{disabled_at}</td>\
        <td>{rotate_btn}</td></tr>",
        user = he(&view.user_name),
        proj = he(view.project.as_deref().unwrap_or("—")),
        label = he(view.label.as_deref().unwrap_or("—")),
        created = view.created_at,
    )
}

/// Build a KeyView from an ApiKey + user_name.
fn to_key_view(k: &crate::db::models::ApiKey, user_name: String) -> KeyView {
    KeyView {
        id: k.id,
        user_id: k.user_id,
        user_name,
        project: k.project.clone(),
        label: k.label.clone(),
        enabled: k.enabled,
        created_at: k.created_at.clone(),
        disabled_at: k.disabled_at.clone(),
        raw_key: None,
    }
}

/// Render all keys for a group (fetched fresh from DB) as HTML.
async fn render_group(
    db: &dyn crate::api::app::DatabaseProvider,
    user_id: i64,
    project: Option<&str>,
    expanded: bool,
) -> Result<String, DashboardError> {
    use crate::db::repositories::{api_keys::ApiKeyRepository, users::UserRepository};
    let group_keys = db.list_keys_for_group(user_id, project)
        .await.map_err(|_| DashboardError::Internal)?;
    if group_keys.is_empty() {
        return Ok(String::new());
    }
    let users = UserRepository::list(db).await.map_err(|_| DashboardError::Internal)?;
    let user_map: std::collections::HashMap<i64, String> =
        users.iter().map(|u| (u.id, u.name.clone())).collect();
    let get_name = |uid: i64| user_map.get(&uid).cloned().unwrap_or_else(|| format!("user:{uid}"));

    let active = group_keys.iter().find(|k| k.enabled)
        .map(|k| to_key_view(k, get_name(k.user_id)));
    let disabled_views: Vec<KeyView> = group_keys.iter()
        .filter(|k| !k.enabled)
        .map(|k| to_key_view(k, get_name(k.user_id)))
        .collect();

    let group_id = active.as_ref().map(|k| k.id)
        .or_else(|| disabled_views.first().map(|k| k.id))
        .unwrap_or(0);
    Ok(key_group_html(group_id, active.as_ref(), &disabled_views, expanded))
}

pub async fn get_keys(
    State(state): State<AppState>,
    session: DashboardSession,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::{api_keys::ApiKeyRepository, users::UserRepository};
    use std::collections::HashMap;

    let keys = state.db.list_all_api_keys()
        .await.map_err(|_| DashboardError::Internal)?;
    let users = UserRepository::list(&*state.db)
        .await.map_err(|_| DashboardError::Internal)?;

    let user_map: HashMap<i64, String> = users.iter().map(|u| (u.id, u.name.clone())).collect();
    let get_name = |uid: i64| user_map.get(&uid).cloned().unwrap_or_else(|| format!("user:{uid}"));

    // Collect datalist values
    let mut user_names: Vec<String> = users.iter().map(|u| u.name.clone()).collect();
    user_names.sort();
    let mut projects: Vec<String> = Vec::new();

    // Group keys by (user_id, project) preserving order from list_all (enabled DESC, created_at DESC)
    let mut seen_groups: Vec<(i64, Option<String>)> = Vec::new();
    let mut group_map: HashMap<(i64, Option<String>), Vec<&crate::db::models::ApiKey>> = HashMap::new();
    for k in &keys {
        let gk = (k.user_id, k.project.clone());
        if !group_map.contains_key(&gk) { seen_groups.push(gk.clone()); }
        group_map.entry(gk).or_default().push(k);
        if let Some(p) = &k.project { if !projects.contains(p) { projects.push(p.clone()); } }
    }

    // Render each group as a <tbody>
    let groups_html: String = seen_groups.iter().filter_map(|gk| {
        let group_keys = &group_map[gk];
        let active = group_keys.iter().find(|k| k.enabled)
            .map(|k| to_key_view(k, get_name(k.user_id)));
        let disabled_views: Vec<KeyView> = group_keys.iter()
            .filter(|k| !k.enabled)
            .map(|k| to_key_view(k, get_name(k.user_id)))
            .collect();
        let group_id = active.as_ref().map(|k| k.id)
            .or_else(|| disabled_views.first().map(|k| k.id))?;
        Some(key_group_html(group_id, active.as_ref(), &disabled_views, false))
    }).collect();

    render(
        "keys.html",
        minijinja::context! {
            groups_html => groups_html,
            user_names => user_names,
            projects => projects,
            session => minijinja::context! {
                user_name => session.0.name.clone(),
                role => session.0.role.clone(),
            },
        },
    )
}

pub async fn post_create_key(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Form(form): Form<CreateKeyForm>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::models::{NewApiKey, NewUser};
    use crate::db::repositories::users::UserRepository;
    use crate::api::auth::hash_token;

    if form.user_name.trim().is_empty() {
        return Err(DashboardError::BadRequest("user_name is required".into()));
    }

    let user_name = form.user_name.trim().to_string();

    // Find or create user
    let user = match UserRepository::find_by_name(&*state.db, &user_name).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            match UserRepository::create(&*state.db, NewUser {
                name: user_name.clone(),
                group_name: None,
                email: None,
            }).await {
                Ok(u) => u,
                Err(_) => {
                    UserRepository::find_by_name(&*state.db, &user_name)
                        .await.map_err(|_| DashboardError::Internal)?
                        .ok_or(DashboardError::Internal)?
                }
            }
        }
        Err(_) => return Err(DashboardError::Internal),
    };

    let label = if form.label.trim().is_empty() { None } else { Some(form.label.trim().to_string()) };
    let project = if form.project.trim().is_empty() { None } else { Some(form.project.trim().to_string()) };

    // Reject duplicate user+project combos (enabled or disabled)
    if let Some(existing) = state.db
        .find_key_by_user_project(user.id, project.as_deref())
        .await.map_err(|_| DashboardError::Internal)?
    {
        let proj_label = project.as_deref().unwrap_or("(no project)");
        let status_tag = if existing.enabled {
            "<span class=\"tag tag-enabled\">Enabled</span>"
        } else {
            "<span class=\"tag tag-disabled\">Disabled</span>"
        };
        // Find the group id (active key's id or the existing key's id)
        let group_id = existing.id.to_string();
        let msg = format!(
            "<div class=\"alert alert-warning\" style=\"display:flex;align-items:center;gap:1rem;\">\
              <span>A key for <strong>{user}</strong> / project <strong>{proj}</strong> already exists ({status}). \
              Scroll down to see it, or rotate it to generate a new secret.</span>\
              <button class=\"btn btn-secondary\" \
                hx-post=\"/admin/keys/{gid}/rotate\" \
                hx-target=\"#key-group-{gid}\" hx-swap=\"outerHTML\" \
                onclick=\"document.getElementById('key-form-message').innerHTML=''\"\
              >Rotate existing key</button>\
            </div>\
            <script>(function(){{\
              var el=document.getElementById('key-group-{gid}');\
              if(el){{el.scrollIntoView({{behavior:'smooth',block:'center'}});\
              el.classList.add('row-highlight');\
              setTimeout(function(){{el.classList.remove('row-highlight');}},2500);}}\
            }})();</script>",
            user = he(&user_name),
            proj = he(proj_label),
            status = status_tag,
            gid = group_id,
        );
        return Ok(Html(msg));
    }

    let raw_key = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let key_hash = hash_token(&raw_key);

    let new_key = state.db.create_api_key(NewApiKey {
        user_id: user.id,
        key_hash,
        label: label.clone(),
        expires_at: None,
        project: project.clone(),
    })
    .await.map_err(|_| DashboardError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "key.create",
        Some(format!("key:{}", new_key.id)),
        None,
        Some(serde_json::json!({ "user_id": user.id, "project": project, "label": label }).to_string()),
    )
    .await;

    // Render the new group tbody (no raw_key in table row)
    let view = to_key_view(&new_key, user.name.clone());
    let tbody = key_group_html(view.id, Some(&view), &[], false);

    // Show the one-time raw key in the message area, inject the group into the table via script
    let proj_str = project.as_ref().map(|p| format!(" / {p}")).unwrap_or_default();
    let raw_e = he(&raw_key);
    let html = format!(
        "<div class=\"alert\" style=\"background:#d4edda;border:1px solid #c3e6cb;color:#155724;\
        display:flex;align-items:center;gap:1rem;flex-wrap:wrap;\">\
        <span>✓ Key created for <strong>{user}</strong>{proj_str}. Copy it now — it won't be shown again:</span>\
        <code style=\"font-family:monospace;background:#f0fff0;padding:0.2rem 0.4rem;\
        border-radius:3px;border:1px solid #c3e6cb;\">{raw_e}</code>\
        <button type=\"button\" onclick=\"navigator.clipboard.writeText('{raw_e}').then(function(){{\
        var b=this;b.textContent='✓ Copied';setTimeout(function(){{b.textContent='⧉ Copy'}},1500)\
        }}.bind(this))\" class=\"btn btn-secondary\">⧉ Copy</button>\
        </div>\
        <template id=\"__new_group__\">{tbody}</template>\
        <script>(function(){{\
        var t=document.getElementById('__new_group__');\
        var el=t.content.firstElementChild.cloneNode(true);\
        var table=document.getElementById('keys-table');\
        var first=table.querySelector('tbody');\
        table.insertBefore(el,first||null);\
        htmx.process(el);\
        t.remove();\
        }})();</script>",
        user = he(&user.name),
        proj_str = proj_str,
        raw_e = raw_e,
        tbody = tbody,
    );
    Ok(Html(html))
}

pub async fn post_disable_key(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::api_keys::ApiKeyRepository;

    // Look up key to get user_id + project before disabling
    let all = state.db.list_all_api_keys().await.map_err(|_| DashboardError::Internal)?;
    let key = all.iter().find(|k| k.id == id)
        .ok_or_else(|| DashboardError::NotFound(format!("key {id} not found")))?;
    let (user_id, project) = (key.user_id, key.project.clone());

    state.db.disable_key(id).await.map_err(|_| DashboardError::Internal)?;

    audit(&state.db, Some(session.0.sub), &session.0.name, "key.disable",
        Some(format!("key:{id}")), None,
        Some(serde_json::json!({"enabled": false}).to_string()),
    ).await;

    Ok(Html(render_group(&*state.db, user_id, project.as_deref(), false).await?))
}

pub async fn post_rotate_key(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    use crate::db::models::NewApiKey;
    use crate::db::repositories::api_keys::ApiKeyRepository;
    use crate::api::auth::hash_token;

    // Look up key to get user_id, project, label
    let all = state.db.list_all_api_keys().await.map_err(|_| DashboardError::Internal)?;
    let old_key = all.iter().find(|k| k.id == id)
        .ok_or_else(|| DashboardError::NotFound(format!("key {id} not found")))?;
    let (user_id, project, label) = (old_key.user_id, old_key.project.clone(), old_key.label.clone());

    // Disable all currently active keys for this group, then create a new one
    let group_keys = state.db.list_keys_for_group(user_id, project.as_deref())
        .await.map_err(|_| DashboardError::Internal)?;
    for k in group_keys.iter().filter(|k| k.enabled) {
        state.db.disable_key(k.id).await.map_err(|_| DashboardError::Internal)?;
    }

    let raw_key = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let key_hash = hash_token(&raw_key);
    let new_key = state.db.create_api_key(NewApiKey {
        user_id, key_hash, label, expires_at: None, project: project.clone(),
    }).await.map_err(|_| DashboardError::Internal)?;

    audit(&state.db, Some(session.0.sub), &session.0.name, "key.rotate",
        Some(format!("key:{}", new_key.id)), None,
        Some(serde_json::json!({ "user_id": user_id, "replaced_key_id": id }).to_string()),
    ).await;

    // Fetch updated group and render expanded (so user sees the newly disabled old key)
    let group_keys = state.db.list_keys_for_group(user_id, project.as_deref())
        .await.map_err(|_| DashboardError::Internal)?;
    use crate::db::repositories::users::UserRepository;
    let users = UserRepository::list(&*state.db).await.map_err(|_| DashboardError::Internal)?;
    let user_map: std::collections::HashMap<i64, String> =
        users.iter().map(|u| (u.id, u.name.clone())).collect();
    let get_name = |uid: i64| user_map.get(&uid).cloned().unwrap_or_else(|| format!("user:{uid}"));

    let active_view = group_keys.iter().find(|k| k.enabled).map(|k| {
        let mut v = to_key_view(k, get_name(k.user_id));
        // Show raw key on the new active row
        v.raw_key = Some(raw_key.clone());
        v
    });
    let disabled_views: Vec<KeyView> = group_keys.iter()
        .filter(|k| !k.enabled)
        .map(|k| to_key_view(k, get_name(k.user_id)))
        .collect();

    let group_id = active_view.as_ref().map(|k| k.id).unwrap_or(disabled_views[0].id);
    Ok(Html(key_group_html(group_id, active_view.as_ref(), &disabled_views, true)))
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
