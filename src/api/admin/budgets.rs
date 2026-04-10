use axum::{extract::State, response::Html};
use axum::extract::Form;
use serde::Deserialize;
use crate::api::app::AppState;
use super::dashboard::{DashboardError, DashboardSession, SuperDashboardSession};
use crate::db::models::{BudgetRule, BudgetScope, UpdateBudgetRule, NewBudgetRule};
use crate::db::repositories::budgets::BudgetRepository;

fn he(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn rule_row_html(rule: &BudgetRule, card_target: &str) -> String {
    let window_label = match rule.window.as_str() {
        "monthly" => "Monthly".to_string(),
        "total" => {
            let start = rule.window_start.as_deref().unwrap_or("?");
            let end = rule.window_end.as_deref().unwrap_or("?");
            format!("Total ({} – {})", start, end)
        }
        "target" => "Target".to_string(),
        w => he(w),
    };
    let limit_usd = rule.limit_usd.map(|v| format!("${:.2}", v)).unwrap_or_else(|| "—".to_string());
    let limit_tokens = rule.limit_tokens.map(|v| v.to_string()).unwrap_or_else(|| "—".to_string());
    let rate_rpm = rule.rate_rpm.map(|v| v.to_string()).unwrap_or_else(|| "—".to_string());
    let model_allow: Vec<String> = serde_json::from_str(&rule.model_allow).unwrap_or_default();
    let model_deny: Vec<String> = serde_json::from_str(&rule.model_deny).unwrap_or_default();
    let model_allow_str = if model_allow.is_empty() { "—".to_string() } else { model_allow.join(", ") };
    let model_deny_str = if model_deny.is_empty() { "—".to_string() } else { model_deny.join(", ") };

    let edit_form = if rule.window == "target" {
        format!(
            r##"<form hx-post="/admin/budgets/{id}/edit" hx-target="{target}" hx-swap="outerHTML" style="display:inline;">
                <input type="number" name="limit_usd" step="0.01" value="{lusd}" style="width:80px;padding:0.25rem;">
                <button type="submit" class="btn btn-secondary" style="font-size:0.8rem;padding:0.3rem 0.6rem;">Save</button>
            </form>"##,
            id = rule.id,
            target = card_target,
            lusd = rule.limit_usd.unwrap_or(0.0),
        )
    } else {
        let date_inputs = if rule.window == "total" {
            format!(
                r#"<input type="date" name="window_start" value="{ws}" style="padding:0.25rem;border:1px solid #ccc;border-radius:4px;">
                   <input type="date" name="window_end" value="{we}" style="padding:0.25rem;border:1px solid #ccc;border-radius:4px;">"#,
                ws = rule.window_start.as_deref().and_then(|s| s.get(..10)).unwrap_or(""),
                we = rule.window_end.as_deref().and_then(|s| s.get(..10)).unwrap_or(""),
            )
        } else {
            String::new()
        };
        format!(
            r##"<form hx-post="/admin/budgets/{id}/edit" hx-target="{target}" hx-swap="outerHTML" style="display:flex;gap:0.3rem;align-items:center;flex-wrap:wrap;">
                <input type="number" name="limit_usd" step="0.01" placeholder="USD" value="{lusd}" style="width:80px;padding:0.25rem;border:1px solid #ccc;border-radius:4px;" title="Limit USD">
                <input type="number" name="limit_tokens" placeholder="Tokens" value="{ltok}" style="width:80px;padding:0.25rem;border:1px solid #ccc;border-radius:4px;" title="Limit Tokens">
                <input type="number" name="rate_rpm" placeholder="RPM" value="{rpm}" style="width:60px;padding:0.25rem;border:1px solid #ccc;border-radius:4px;" title="Rate RPM">
                {date_inputs}
                <button type="submit" class="btn btn-secondary" style="font-size:0.8rem;padding:0.3rem 0.6rem;">Save</button>
            </form>"##,
            id = rule.id,
            target = card_target,
            lusd = rule.limit_usd.map(|v| v.to_string()).unwrap_or_default(),
            ltok = rule.limit_tokens.map(|v| v.to_string()).unwrap_or_default(),
            rpm = rule.rate_rpm.map(|v| v.to_string()).unwrap_or_default(),
            date_inputs = date_inputs,
        )
    };
    let delete_btn = format!(
        r##"<button class="btn btn-danger" style="font-size:0.8rem;padding:0.3rem 0.6rem;" hx-post="/admin/budgets/{id}/delete" hx-target="{target}" hx-swap="outerHTML" hx-confirm="Delete this budget rule?">Delete</button>"##,
        id = rule.id,
        target = card_target,
    );
    format!(
        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td style='display:flex;gap:0.4rem;'>{}{}</td></tr>",
        window_label, limit_usd, limit_tokens, rate_rpm, he(&model_allow_str), he(&model_deny_str), edit_form, delete_btn
    )
}

fn add_rule_form_html(card_target: &str, scope_fields: &str) -> String {
    format!(
        r##"<form hx-post="/admin/budgets" hx-target="{target}" hx-swap="outerHTML" style="display:flex;gap:0.5rem;flex-wrap:wrap;align-items:flex-end;margin-top:0.75rem;padding-top:0.75rem;border-top:1px solid #eee;">
            {scope_fields}
            <div>
                <label style="display:block;font-size:0.8rem;margin-bottom:0.2rem;">Window</label>
                <select name="window" style="padding:0.3rem;border:1px solid #ccc;border-radius:4px;font-size:0.85rem;">
                    <option value="monthly">Monthly</option>
                    <option value="total">Total (date range)</option>
                </select>
            </div>
            <div>
                <label style="display:block;font-size:0.8rem;margin-bottom:0.2rem;">Limit USD</label>
                <input type="number" name="limit_usd" step="0.01" placeholder="100.00" style="width:90px;padding:0.3rem;border:1px solid #ccc;border-radius:4px;">
            </div>
            <div>
                <label style="display:block;font-size:0.8rem;margin-bottom:0.2rem;">Start (total only)</label>
                <input type="date" name="window_start" style="padding:0.3rem;border:1px solid #ccc;border-radius:4px;">
            </div>
            <div>
                <label style="display:block;font-size:0.8rem;margin-bottom:0.2rem;">End (total only)</label>
                <input type="date" name="window_end" style="padding:0.3rem;border:1px solid #ccc;border-radius:4px;">
            </div>
            <button type="submit" class="btn btn-primary" style="font-size:0.85rem;">Add Rule</button>
        </form>"##,
        target = card_target,
    )
}

fn budget_card_html(card_id: &str, title: &str, rules: &[BudgetRule], scope_fields: &str) -> String {
    let card_target = format!("#{}", card_id);
    let rows: String = if rules.is_empty() {
        "<tr><td colspan='7' style='color:#999;text-align:center;padding:0.5rem;'>No rules yet.</td></tr>".to_string()
    } else {
        rules.iter().map(|r| rule_row_html(r, &card_target)).collect()
    };
    let add_form = add_rule_form_html(&card_target, scope_fields);

    format!(
        r#"<div id="{card_id}" style="background:#fff;border-radius:6px;padding:1.25rem;box-shadow:0 1px 3px rgba(0,0,0,0.1);margin-bottom:1rem;">
            <h3 style="margin-bottom:0.75rem;font-size:1rem;">{title}</h3>
            <table style="width:100%;border-collapse:collapse;">
                <thead>
                    <tr style="font-size:0.8rem;text-transform:uppercase;color:#777;">
                        <th>Window</th><th>Limit USD</th><th>Limit Tokens</th>
                        <th>Rate RPM</th><th>Allow Models</th><th>Deny Models</th><th>Actions</th>
                    </tr>
                </thead>
                <tbody>{rows}</tbody>
            </table>
            {add_form}
        </div>"#,
        card_id = card_id,
        title = he(title),
        rows = rows,
        add_form = add_form,
    )
}

pub async fn get_budgets(
    State(state): State<AppState>,
    _session: DashboardSession,
) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::users::UserRepository;
    use crate::db::repositories::groups::GroupRepository;
    use crate::db::repositories::api_keys::ApiKeyRepository;

    let all_rules = BudgetRepository::list_all(&*state.db)
        .await
        .map_err(|_| DashboardError::Internal)?;

    // Global tab
    let global_rules: Vec<BudgetRule> = all_rules.iter()
        .filter(|r| r.user_id.is_none() && r.group_name.is_none() && r.project.is_none() && r.api_key_id.is_none())
        .cloned().collect();
    let global_card = budget_card_html("budget-card-global", "Global (Org-Wide)", &global_rules,
        r#"<input type="hidden" name="scope" value="global">"#);

    // Projects tab — union of api_keys.project + budget_rules.project
    let all_keys = ApiKeyRepository::list_all_api_keys(&*state.db)
        .await
        .map_err(|_| DashboardError::Internal)?;
    let mut project_names: std::collections::BTreeSet<String> = all_keys.iter()
        .filter_map(|k| k.project.clone())
        .collect();
    for r in &all_rules {
        if let Some(p) = &r.project { project_names.insert(p.clone()); }
    }
    let mut project_cards = String::new();
    for proj in &project_names {
        let proj_rules: Vec<BudgetRule> = all_rules.iter()
            .filter(|r| r.project.as_deref() == Some(proj.as_str()))
            .cloned().collect();
        let proj_slug = proj.chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect::<String>();
        let card_id = format!("budget-card-project-{}", proj_slug);
        let scope_fields = format!(
            r#"<input type="hidden" name="scope" value="project"><input type="hidden" name="project" value="{}">"#,
            he(proj)
        );
        project_cards.push_str(&budget_card_html(&card_id, proj, &proj_rules, &scope_fields));
    }

    // Users tab
    let all_users = UserRepository::list(&*state.db)
        .await
        .map_err(|_| DashboardError::Internal)?;
    let mut user_cards = String::new();
    for user in &all_users {
        let user_rules: Vec<BudgetRule> = all_rules.iter()
            .filter(|r| r.user_id == Some(user.id))
            .cloned().collect();
        let card_id = format!("budget-card-user-{}", user.id);
        let scope_fields = format!(
            r#"<input type="hidden" name="scope" value="user"><input type="hidden" name="user_id" value="{}">"#,
            user.id
        );
        user_cards.push_str(&budget_card_html(&card_id, &user.name, &user_rules, &scope_fields));
    }

    // Groups tab — groups + orphan budget rules
    let groups = GroupRepository::list_groups(&*state.db)
        .await
        .map_err(|_| DashboardError::Internal)?;
    let group_names: std::collections::HashSet<String> = groups.iter().map(|g| g.name.clone()).collect();
    let mut group_cards = String::new();
    for group in &groups {
        let grp_rules: Vec<BudgetRule> = all_rules.iter()
            .filter(|r| r.group_name.as_deref() == Some(group.name.as_str()))
            .cloned().collect();
        let card_id = format!("budget-card-group-{}", he(&group.name));
        let target_html = if let Some(rule) = grp_rules.first() {
            let edit_target = format!("#{}", card_id);
            format!(
                r##"<p style="margin:0.5rem 0;">Target: <strong>${:.2}</strong>
                    <button class="btn btn-danger" style="font-size:0.8rem;padding:0.2rem 0.5rem;margin-left:0.5rem;" hx-post="/admin/budgets/{id}/delete" hx-target="#{cid}" hx-swap="outerHTML" hx-confirm="Remove group target?">Remove</button>
                    {edit}
                </p>"##,
                rule.limit_usd.unwrap_or(0.0),
                id = rule.id,
                cid = card_id,
                edit = format!(
                    r##"<form hx-post="/admin/budgets/{id}/edit" hx-target="{etarget}" hx-swap="outerHTML" style="display:inline;">
                        <input type="number" name="limit_usd" step="0.01" value="{lusd}" style="width:80px;padding:0.25rem;">
                        <button type="submit" class="btn btn-secondary" style="font-size:0.8rem;padding:0.3rem 0.6rem;">Save</button>
                    </form>"##,
                    id = rule.id,
                    etarget = edit_target,
                    lusd = rule.limit_usd.unwrap_or(0.0),
                )
            )
        } else {
            let scope_fields = format!(
                r#"<input type="hidden" name="scope" value="group"><input type="hidden" name="group_name" value="{}">"#,
                he(&group.name)
            );
            format!(
                r##"<form hx-post="/admin/budgets" hx-target="#{cid}" hx-swap="outerHTML" style="display:flex;gap:0.5rem;align-items:flex-end;margin-top:0.5rem;">
                    {scope_fields}
                    <input type="hidden" name="window" value="target">
                    <div>
                        <label style="display:block;font-size:0.8rem;margin-bottom:0.2rem;">Target USD</label>
                        <input type="number" name="limit_usd" step="0.01" placeholder="1000.00" style="width:100px;padding:0.3rem;border:1px solid #ccc;border-radius:4px;">
                    </div>
                    <button type="submit" class="btn btn-primary" style="font-size:0.85rem;">Set Target</button>
                </form>"##,
                cid = card_id,
            )
        };
        group_cards.push_str(&format!(
            r#"<div id="{card_id}" style="background:#fff;border-radius:6px;padding:1.25rem;box-shadow:0 1px 3px rgba(0,0,0,0.1);margin-bottom:1rem;">
                <h3 style="margin-bottom:0.5rem;font-size:1rem;">{name} <span style="font-size:0.75rem;color:#777;">(soft target)</span></h3>
                {target_html}
            </div>"#,
            card_id = card_id,
            name = he(&group.name),
            target_html = target_html,
        ));
    }
    // Orphaned group rules
    for r in all_rules.iter().filter(|r| r.group_name.is_some()) {
        let gn = r.group_name.as_deref().unwrap();
        if !group_names.contains(gn) {
            let card_id = format!("budget-card-group-orphan-{}", r.id);
            group_cards.push_str(&format!(
                r##"<div id="{card_id}" style="background:#fff3cd;border-radius:6px;padding:1rem;box-shadow:0 1px 3px rgba(0,0,0,0.1);margin-bottom:1rem;">
                    <strong style="color:#856404;">Group not found: {gn}</strong>
                    — <span style="font-size:0.85rem;">Target ${lusd:.2}</span>
                    <button class="btn btn-danger" style="font-size:0.8rem;padding:0.2rem 0.5rem;margin-left:0.5rem;" hx-post="/admin/budgets/{id}/delete" hx-target="#{card_id}" hx-swap="outerHTML" hx-confirm="Delete orphaned rule?">Delete</button>
                </div>"##,
                card_id = card_id,
                gn = he(gn),
                lusd = r.limit_usd.unwrap_or(0.0),
                id = r.id,
            ));
        }
    }

    super::dashboard::render(
        "budgets.html",
        minijinja::context! {
            global_card => global_card,
            project_cards => project_cards,
            user_cards => user_cards,
            group_cards => group_cards,
        },
    )
}

#[derive(Deserialize)]
pub struct CreateBudgetForm {
    pub scope: String,
    pub project: Option<String>,
    pub user_id: Option<i64>,
    pub group_name: Option<String>,
    pub window: String,
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<i64>,
    pub rate_rpm: Option<i64>,
    pub max_concurrent: Option<i64>,
    pub window_start: Option<String>,
    pub window_end: Option<String>,
}

pub async fn post_create_budget(
    State(state): State<AppState>,
    _session: SuperDashboardSession,
    Form(form): Form<CreateBudgetForm>,
) -> Result<Html<String>, DashboardError> {
    if form.window == "total" {
        match (&form.window_start, &form.window_end) {
            (Some(s), Some(e)) if s < e => {}
            _ => return Ok(Html(
                r#"<div class="alert alert-danger">Total window requires start &lt; end dates.</div>"#.to_string()
            )),
        }
    }

    if form.window == "target" && form.scope != "group" {
        return Ok(Html(r#"<div class="alert alert-danger">Target window is only for group rules.</div>"#.to_string()));
    }

    let existing_scope = match form.scope.as_str() {
        "global" => BudgetScope::Global,
        "project" => BudgetScope::Project(form.project.clone().unwrap_or_default()),
        "user" => BudgetScope::User(form.user_id.unwrap_or(0)),
        "group" => BudgetScope::Group(form.group_name.clone().unwrap_or_default()),
        _ => return Ok(Html(r#"<div class="alert alert-danger">Invalid scope.</div>"#.to_string())),
    };
    let existing = BudgetRepository::list_for_scope(&*state.db, &existing_scope)
        .await
        .map_err(|_| DashboardError::Internal)?;
    if existing.iter().any(|r| r.window == form.window) {
        let card = render_scope_card(&state, &existing_scope).await?;
        return Ok(Html(format!(
            r#"<div class="alert alert-danger" style="margin-bottom:0.75rem;">A {} rule already exists for this scope.</div>{}"#,
            he(&form.window),
            card.0
        )));
    }

    let (user_id, group_name, project) = match &existing_scope {
        BudgetScope::Global => (None, None, None),
        BudgetScope::Project(p) => (None, None, Some(p.clone())),
        BudgetScope::User(uid) => (Some(*uid), None, None),
        BudgetScope::Group(gn) => (None, Some(gn.clone()), None),
    };

    let window_start = form.window_start.as_deref().map(|d| format!("{}T00:00:00+00:00", d));
    let window_end = form.window_end.as_deref().map(|d| format!("{}T00:00:00+00:00", d));

    BudgetRepository::create(&*state.db, NewBudgetRule {
        user_id,
        group_name,
        api_key_id: None,
        tag: None,
        project,
        window: form.window.clone(),
        limit_usd: form.limit_usd,
        limit_tokens: form.limit_tokens,
        model_allow: vec![],
        model_deny: vec![],
        rate_rpm: form.rate_rpm,
        max_concurrent: form.max_concurrent,
        window_start,
        window_end,
    }).await.map_err(|_| DashboardError::Internal)?;

    render_scope_card(&state, &existing_scope).await
}

#[derive(Deserialize)]
pub struct EditBudgetForm {
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<i64>,
    pub rate_rpm: Option<i64>,
    pub max_concurrent: Option<i64>,
    pub window_start: Option<String>,
    pub window_end: Option<String>,
}

pub async fn post_edit_budget(
    State(state): State<AppState>,
    _session: SuperDashboardSession,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Form(form): Form<EditBudgetForm>,
) -> Result<Html<String>, DashboardError> {
    BudgetRepository::update(&*state.db, id, &UpdateBudgetRule {
        limit_usd: form.limit_usd,
        limit_tokens: form.limit_tokens,
        model_allow: None,
        model_deny: None,
        rate_rpm: form.rate_rpm,
        max_concurrent: form.max_concurrent,
        window_start: form.window_start,
        window_end: form.window_end,
    }).await.map_err(|_| DashboardError::Internal)?;

    let scope = scope_for_rule_id(&state, id).await?;
    render_scope_card(&state, &scope).await
}

pub async fn post_delete_budget(
    State(state): State<AppState>,
    _session: SuperDashboardSession,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Html<String>, DashboardError> {
    let scope = scope_for_rule_id(&state, id).await?;
    BudgetRepository::delete(&*state.db, id).await.map_err(|_| DashboardError::Internal)?;
    render_scope_card(&state, &scope).await
}

async fn scope_for_rule_id(state: &AppState, id: i64) -> Result<BudgetScope, DashboardError> {
    let all = BudgetRepository::list_all(&*state.db)
        .await
        .map_err(|_| DashboardError::Internal)?;
    let rule = all.iter().find(|r| r.id == id)
        .ok_or_else(|| DashboardError::NotFound("budget rule not found".to_string()))?;
    let scope = if rule.user_id.is_some() {
        BudgetScope::User(rule.user_id.unwrap())
    } else if rule.group_name.is_some() {
        BudgetScope::Group(rule.group_name.clone().unwrap())
    } else if rule.project.is_some() {
        BudgetScope::Project(rule.project.clone().unwrap())
    } else {
        BudgetScope::Global
    };
    Ok(scope)
}

async fn render_scope_card(state: &AppState, scope: &BudgetScope) -> Result<Html<String>, DashboardError> {
    use crate::db::repositories::users::UserRepository;

    let rules = BudgetRepository::list_for_scope(&*state.db, scope)
        .await
        .map_err(|_| DashboardError::Internal)?;

    let (card_id, title, scope_fields) = match scope {
        BudgetScope::Global => (
            "budget-card-global".to_string(),
            "Global (Org-Wide)".to_string(),
            r#"<input type="hidden" name="scope" value="global">"#.to_string(),
        ),
        BudgetScope::Project(p) => {
            let slug = p.chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect::<String>();
            (
                format!("budget-card-project-{}", slug),
                p.clone(),
                format!(r#"<input type="hidden" name="scope" value="project"><input type="hidden" name="project" value="{}">"#, he(p)),
            )
        }
        BudgetScope::User(uid) => {
            let user_name = UserRepository::find_by_id(&*state.db, *uid)
                .await
                .map(|u| u.map(|u| u.name))
                .unwrap_or(None)
                .unwrap_or_else(|| format!("User #{}", uid));
            (
                format!("budget-card-user-{}", uid),
                user_name,
                format!(r#"<input type="hidden" name="scope" value="user"><input type="hidden" name="user_id" value="{}">"#, uid),
            )
        }
        BudgetScope::Group(gn) => {
            let card_id = format!("budget-card-group-{}", he(gn));
            let card_target = format!("#{}", card_id);
            let target_html = if let Some(rule) = rules.first() {
                format!(
                    r##"<p style="margin:0.5rem 0;">Target: <strong>${:.2}</strong>
                        <button class="btn btn-danger" style="font-size:0.8rem;padding:0.2rem 0.5rem;margin-left:0.5rem;" hx-post="/admin/budgets/{id}/delete" hx-target="{target}" hx-swap="outerHTML" hx-confirm="Remove group target?">Remove</button>
                        <form hx-post="/admin/budgets/{id}/edit" hx-target="{target}" hx-swap="outerHTML" style="display:inline;">
                            <input type="number" name="limit_usd" step="0.01" value="{lusd}" style="width:80px;padding:0.25rem;">
                            <button type="submit" class="btn btn-secondary" style="font-size:0.8rem;padding:0.3rem 0.6rem;">Save</button>
                        </form>
                    </p>"##,
                    rule.limit_usd.unwrap_or(0.0),
                    id = rule.id,
                    target = card_target,
                    lusd = rule.limit_usd.unwrap_or(0.0),
                )
            } else {
                format!(
                    r##"<form hx-post="/admin/budgets" hx-target="{target}" hx-swap="outerHTML" style="display:flex;gap:0.5rem;align-items:flex-end;margin-top:0.5rem;">
                        <input type="hidden" name="scope" value="group">
                        <input type="hidden" name="group_name" value="{gn_escaped}">
                        <input type="hidden" name="window" value="target">
                        <div>
                            <label style="display:block;font-size:0.8rem;margin-bottom:0.2rem;">Target USD</label>
                            <input type="number" name="limit_usd" step="0.01" placeholder="1000.00" style="width:100px;padding:0.3rem;border:1px solid #ccc;border-radius:4px;">
                        </div>
                        <button type="submit" class="btn btn-primary" style="font-size:0.85rem;">Set Target</button>
                    </form>"##,
                    target = card_target,
                    gn_escaped = he(gn),
                )
            };
            return Ok(Html(format!(
                r#"<div id="{card_id}" style="background:#fff;border-radius:6px;padding:1.25rem;box-shadow:0 1px 3px rgba(0,0,0,0.1);margin-bottom:1rem;">
                    <h3 style="margin-bottom:0.5rem;font-size:1rem;">{name} <span style="font-size:0.75rem;color:#777;">(soft target)</span></h3>
                    {target_html}
                </div>"#,
                card_id = card_id,
                name = he(gn),
                target_html = target_html,
            )));
        }
    };

    Ok(Html(budget_card_html(&card_id, &title, &rules, &scope_fields)))
}
