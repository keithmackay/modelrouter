use axum::{
    extract::{Path, State},
    response::Html,
    Form,
};
use serde::Deserialize;

use super::audit::audit;
use super::dashboard::{DashboardError, DashboardSession, SuperDashboardSession};
use super::templates::fmt_ts;
use crate::api::app::AppState;
use crate::db::models::{Group, GroupMembership};
use crate::db::repositories::{groups::GroupRepository, users::UserRepository};

// ── Template helpers ──────────────────────────────────────────────────────────

fn he(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn membership_rows_html(memberships: &[GroupMembership], group_id: i64) -> String {
    if memberships.is_empty() {
        return r#"<tr><td colspan="4" style="color:#999;text-align:center;padding:0.5rem;">No members yet.</td></tr>"#.to_string();
    }
    memberships.iter().map(|m| {
        let status = match &m.disabled_at {
            None => r#"<span class="tag tag-enabled">Active</span>"#.to_string(),
            Some(ts) => format!(r#"<span class="tag tag-disabled">Disabled {}</span>"#, he(&fmt_ts(ts))),
        };
        let action = if m.disabled_at.is_none() {
            format!(
                r##"<button class="btn btn-danger" style="font-size:0.8rem;padding:0.3rem 0.7rem;" hx-post="/admin/groups/{}/members/{}/disable" hx-target="#group-card-{}" hx-swap="outerHTML" hx-confirm="Disable {} from this group?">Disable</button>"##,
                group_id, m.user_id, group_id, he(&m.user_name)
            )
        } else {
            String::new()
        };
        format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            he(&m.user_name),
            fmt_ts(&m.joined_at),
            status,
            action,
        )
    }).collect()
}

fn add_member_select_html(
    group_id: i64,
    memberships: &[GroupMembership],
    all_users: &[crate::db::models::User],
) -> String {
    let active_user_ids: std::collections::HashSet<i64> = memberships
        .iter()
        .filter(|m| m.disabled_at.is_none())
        .map(|m| m.user_id)
        .collect();

    let options: String = all_users
        .iter()
        .filter(|u| u.enabled && !active_user_ids.contains(&u.id))
        .map(|u| format!(r#"<option value="{}">{}</option>"#, u.id, he(&u.name)))
        .collect();

    if options.is_empty() {
        return "<span style=\"color:#999;font-size:0.85rem;\">All users are already members.</span>".to_string();
    }

    format!(
        r##"<form hx-post="/admin/groups/{gid}/members" hx-target="#group-card-{gid}" hx-swap="outerHTML" style="display:flex;gap:0.5rem;align-items:center;margin-top:0.75rem;">
            <select name="user_id" style="padding:0.35rem 0.5rem;border:1px solid #ccc;border-radius:4px;font-size:0.85rem;">
                {options}
            </select>
            <button type="submit" class="btn btn-primary" style="font-size:0.85rem;padding:0.35rem 0.7rem;">Add Member</button>
        </form>"##,
        gid = group_id,
    )
}

pub fn group_card_html(
    group: &Group,
    memberships: &[GroupMembership],
    all_users: &[crate::db::models::User],
) -> String {
    let bg = if group.enabled { "#fff" } else { "#f5f5f5" };
    let status_tag = if group.enabled {
        r#"<span class="tag tag-enabled">Enabled</span>"#
    } else {
        r#"<span class="tag tag-disabled">Disabled</span>"#
    };
    let priority_badge = format!(
        r#"<span style="font-size:0.75rem;background:#e8eaf6;color:#3949ab;padding:0.2rem 0.5rem;border-radius:3px;">priority {}</span>"#,
        group.priority
    );

    let member_rows = membership_rows_html(memberships, group.id);

    let add_member_html = if group.enabled {
        add_member_select_html(group.id, memberships, all_users)
    } else {
        String::new()
    };

    let action_btn = if group.enabled {
        format!(
            r##"<button class="btn btn-danger" hx-post="/admin/groups/{}/disable" hx-target="#group-card-{}" hx-swap="outerHTML" hx-confirm="Disable group {}? All active memberships will be disabled.">Disable Group</button>"##,
            group.id, group.id, he(&group.name)
        )
    } else {
        format!(
            r##"<button class="btn btn-success" hx-post="/admin/groups/{}/enable" hx-target="#group-card-{}" hx-swap="outerHTML">Re-enable Group</button>"##,
            group.id, group.id
        )
    };

    format!(
        r#"<div id="group-card-{id}" style="background:{bg};border-radius:6px;box-shadow:0 1px 3px rgba(0,0,0,0.1);padding:1.25rem;margin-bottom:1rem;">
            <div style="display:flex;align-items:center;gap:0.75rem;margin-bottom:1rem;">
                <strong style="font-size:1rem;">{name}</strong>
                {priority_badge}
                {status_tag}
                <span style="flex:1"></span>
                {action_btn}
            </div>
            <table style="width:100%;border-collapse:collapse;font-size:0.85rem;">
                <thead>
                    <tr style="background:#f0f0f0;">
                        <th style="padding:0.4rem 0.75rem;text-align:left;">User</th>
                        <th style="padding:0.4rem 0.75rem;text-align:left;">Joined</th>
                        <th style="padding:0.4rem 0.75rem;text-align:left;">Status</th>
                        <th style="padding:0.4rem 0.75rem;text-align:left;">Actions</th>
                    </tr>
                </thead>
                <tbody>{member_rows}</tbody>
            </table>
            {add_member_html}
        </div>"#,
        id = group.id,
        bg = bg,
        name = he(&group.name),
        priority_badge = priority_badge,
        status_tag = status_tag,
        action_btn = action_btn,
        member_rows = member_rows,
        add_member_html = add_member_html,
    )
}

async fn fetch_card_parts(
    state: &AppState,
    group_id: i64,
) -> Result<(Group, Vec<GroupMembership>, Vec<crate::db::models::User>), DashboardError> {
    let group = GroupRepository::get_group(&*state.db, group_id)
        .await
        .map_err(|_| DashboardError::Internal)?
        .ok_or_else(|| DashboardError::NotFound(format!("group {group_id}")))?;
    let memberships = GroupRepository::list_memberships(&*state.db, group_id)
        .await
        .map_err(|_| DashboardError::Internal)?;
    let all_users = UserRepository::list(&*state.db)
        .await
        .map_err(|_| DashboardError::Internal)?;
    Ok((group, memberships, all_users))
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn get_groups(
    State(state): State<AppState>,
    _session: DashboardSession,
) -> Result<Html<String>, DashboardError> {
    let groups = GroupRepository::list_groups(&*state.db)
        .await
        .map_err(|_| DashboardError::Internal)?;
    let all_users = UserRepository::list(&*state.db)
        .await
        .map_err(|_| DashboardError::Internal)?;

    let mut cards_html = String::new();
    for group in &groups {
        let memberships = GroupRepository::list_memberships(&*state.db, group.id)
            .await
            .map_err(|_| DashboardError::Internal)?;
        cards_html.push_str(&group_card_html(group, &memberships, &all_users));
    }

    super::dashboard::render(
        "groups.html",
        minijinja::context! {
            groups_html => cards_html,
        },
    )
}

#[derive(Deserialize)]
pub struct CreateGroupForm {
    pub name: String,
    pub priority: Option<i64>,
}

pub async fn post_create_group(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Form(form): Form<CreateGroupForm>,
) -> Result<Html<String>, DashboardError> {
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return Err(DashboardError::BadRequest("name is required".into()));
    }

    // Duplicate check
    if GroupRepository::find_group_by_name(&*state.db, &name)
        .await
        .map_err(|_| DashboardError::Internal)?
        .is_some()
    {
        return Ok(Html(format!(
            r#"<div class="alert alert-danger">Group name "{}" already exists.</div>"#,
            he(&name)
        )));
    }

    let priority = form.priority.unwrap_or(0);
    let group = GroupRepository::create_group(&*state.db, &name, priority)
        .await
        .map_err(|_| DashboardError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "group.create",
        Some(format!("group:{}", group.id)),
        None,
        Some(serde_json::json!({ "name": group.name, "priority": group.priority }).to_string()),
    )
    .await;

    let all_users = UserRepository::list(&*state.db)
        .await
        .map_err(|_| DashboardError::Internal)?;
    let card = group_card_html(&group, &[], &all_users);

    // Inject new card into #groups-list via inline script (HTMX can't swap afterbegin on tbody reliably)
    Ok(Html(format!(
        r#"<div></div>
        <script>
            (function() {{
                var list = document.getElementById('groups-list');
                var tmp = document.createElement('div');
                tmp.innerHTML = {card_json};
                list.prepend(tmp.firstElementChild);
                htmx.process(list.firstElementChild);
            }})();
        </script>"#,
        card_json = serde_json::to_string(&card).unwrap_or_default(),
    )))
}

pub async fn post_enable_group(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    GroupRepository::set_group_enabled(&*state.db, id, true)
        .await
        .map_err(|_| DashboardError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "group.enable",
        Some(format!("group:{id}")),
        None,
        Some(r#"{"enabled":true}"#.to_string()),
    )
    .await;

    let (group, memberships, all_users) = fetch_card_parts(&state, id).await?;
    Ok(Html(group_card_html(&group, &memberships, &all_users)))
}

pub async fn post_disable_group(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    GroupRepository::set_group_enabled(&*state.db, id, false)
        .await
        .map_err(|_| DashboardError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "group.disable",
        Some(format!("group:{id}")),
        None,
        Some(r#"{"enabled":false}"#.to_string()),
    )
    .await;

    let (group, memberships, all_users) = fetch_card_parts(&state, id).await?;
    Ok(Html(group_card_html(&group, &memberships, &all_users)))
}

#[derive(Deserialize)]
pub struct AddMemberForm {
    pub user_id: i64,
}

pub async fn post_add_member(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path(group_id): Path<i64>,
    Form(form): Form<AddMemberForm>,
) -> Result<Html<String>, DashboardError> {
    let group = GroupRepository::get_group(&*state.db, group_id)
        .await
        .map_err(|_| DashboardError::Internal)?
        .ok_or_else(|| DashboardError::NotFound(format!("group {group_id}")))?;
    if !group.enabled {
        return Err(DashboardError::BadRequest("cannot add members to a disabled group".into()));
    }

    if GroupRepository::find_active_membership(&*state.db, group_id, form.user_id)
        .await
        .map_err(|_| DashboardError::Internal)?
        .is_some()
    {
        return Err(DashboardError::BadRequest("user is already an active member".into()));
    }

    let membership = GroupRepository::add_member(&*state.db, group_id, form.user_id)
        .await
        .map_err(|_| DashboardError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "group.member.add",
        Some(format!("group:{group_id} user:{}", form.user_id)),
        None,
        Some(serde_json::json!({ "membership_id": membership.id }).to_string()),
    )
    .await;

    let (group, memberships, all_users) = fetch_card_parts(&state, group_id).await?;
    Ok(Html(group_card_html(&group, &memberships, &all_users)))
}

pub async fn post_disable_member(
    State(state): State<AppState>,
    session: SuperDashboardSession,
    Path((group_id, user_id)): Path<(i64, i64)>,
) -> Result<Html<String>, DashboardError> {
    let membership = GroupRepository::find_active_membership(&*state.db, group_id, user_id)
        .await
        .map_err(|_| DashboardError::Internal)?
        .ok_or_else(|| DashboardError::NotFound("active membership".into()))?;

    GroupRepository::disable_membership(&*state.db, membership.id)
        .await
        .map_err(|_| DashboardError::Internal)?;

    audit(
        &state.db,
        Some(session.0.sub),
        &session.0.name,
        "group.member.disable",
        Some(format!("group:{group_id} user:{user_id}")),
        None,
        Some(serde_json::json!({ "membership_id": membership.id }).to_string()),
    )
    .await;

    let (group, memberships, all_users) = fetch_card_parts(&state, group_id).await?;
    Ok(Html(group_card_html(&group, &memberships, &all_users)))
}
