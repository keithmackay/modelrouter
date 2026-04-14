use axum::{extract::{State, Query}, response::Html};
use serde::Deserialize;
use crate::api::app::AppState;
use super::dashboard::{DashboardError, DashboardSession};
use crate::db::repositories::{
    costs::CostRepository,
    users::UserRepository,
    groups::GroupRepository,
    budgets::BudgetRepository,
};

#[derive(Deserialize)]
pub struct ReportsQuery {
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub group: String,
    #[serde(default = "default_window")]
    pub window: String,
}

fn default_window() -> String { "monthly".to_string() }

/// Returns (start, end) as ISO 8601 UTC strings for the given window.
fn window_range(window: &str) -> (String, String) {
    use chrono::{Utc, Datelike, Duration, TimeZone};
    let now = Utc::now();
    let end = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let start = match window {
        "daily" => {
            Utc.from_utc_datetime(&now.date_naive().and_hms_opt(0, 0, 0).unwrap())
                .format("%Y-%m-%dT%H:%M:%SZ").to_string()
        }
        "weekly" => (now - Duration::days(7)).format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        _ => { // monthly — start of current calendar month
            let d = chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap();
            Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
                .format("%Y-%m-%dT%H:%M:%SZ").to_string()
        }
    };
    (start, end)
}

pub async fn get_reports(
    State(state): State<AppState>,
    _session: DashboardSession,
    Query(q): Query<ReportsQuery>,
) -> Result<Html<String>, DashboardError> {
    let users = UserRepository::list(&*state.db).await.map_err(|_| DashboardError::Internal)?;
    let projects = CostRepository::distinct_projects_in_ledger(&*state.db).await.map_err(|_| DashboardError::Internal)?;
    let models = CostRepository::distinct_models_in_ledger(&*state.db).await.map_err(|_| DashboardError::Internal)?;
    let groups = GroupRepository::list_groups(&*state.db).await.map_err(|_| DashboardError::Internal)?;

    let user_opts: Vec<minijinja::Value> = users.iter()
        .map(|u| minijinja::context! { id => u.id, name => u.name.clone() })
        .collect();
    let group_names: Vec<String> = groups.iter().map(|g| g.name.clone()).collect();

    super::dashboard::render("reports.html", minijinja::context! {
        user_opts => user_opts,
        projects => projects,
        models => models,
        group_names => group_names,
        sel_user => q.user,
        sel_project => q.project,
        sel_model => q.model,
        sel_group => q.group,
        sel_window => q.window,
    })
}

pub async fn get_reports_panels(
    State(state): State<AppState>,
    _session: DashboardSession,
    Query(q): Query<ReportsQuery>,
) -> Result<Html<String>, DashboardError> {
    let (start, end) = window_range(&q.window);

    // Resolve group filter → member user IDs
    let mut group_member_ids: Vec<i64> = vec![];
    if !q.group.is_empty() {
        let group = GroupRepository::find_group_by_name(&*state.db, &q.group)
            .await.map_err(|_| DashboardError::Internal)?;
        if let Some(g) = group {
            let members = GroupRepository::list_memberships(&*state.db, g.id)
                .await.map_err(|_| DashboardError::Internal)?;
            group_member_ids = members.into_iter()
                .filter(|m| m.disabled_at.is_none())
                .map(|m| m.user_id)
                .collect();
        }
        // if group not found, group_member_ids stays empty → no results
    }

    // Effective user_ids filter: single user takes priority over group
    let filter_uid: Option<i64> = q.user.parse().ok();
    let eff_user_ids: Option<Vec<i64>> = if let Some(uid) = filter_uid {
        Some(vec![uid])
    } else if !q.group.is_empty() {
        Some(group_member_ids.clone())
    } else {
        None
    };
    let eff_user_ids_ref: Option<&[i64]> = eff_user_ids.as_deref();

    let filter_project = if q.project.is_empty() { None } else { Some(q.project.as_str()) };
    let filter_model_opt = if q.model.is_empty() { None } else { Some(q.model.as_str()) };

    // ── User summary ────────────────────────────────────────────────────────
    let user_stats = CostRepository::cost_stats_grouped(
        &*state.db, eff_user_ids_ref, filter_project, None, &start,
    ).await.map_err(|_| DashboardError::Internal)?;

    let all_users = UserRepository::list(&*state.db).await.map_err(|_| DashboardError::Internal)?;
    let user_name_map: std::collections::HashMap<i64, String> =
        all_users.iter().map(|u| (u.id, u.name.clone())).collect();

    let by_user_rows: Vec<minijinja::Value> = user_stats.iter().map(|(uid, cost, ti, to, rc)| {
        minijinja::context! {
            name => user_name_map.get(uid).cloned().unwrap_or_else(|| format!("#{}", uid)),
            cost_usd => format!("{:.4}", cost),
            tokens_in => *ti,
            tokens_out => *to,
            requests => *rc,
        }
    }).collect();

    // ── Model summary ────────────────────────────────────────────────────────
    let model_rows = CostRepository::summarize_by_model(
        &*state.db, eff_user_ids_ref, filter_project, filter_model_opt, &start,
    ).await.map_err(|_| DashboardError::Internal)?;

    let by_model_rows: Vec<minijinja::Value> = model_rows.iter().map(|r| {
        minijinja::context! {
            model => r.model.clone(),
            cost_usd => format!("{:.4}", r.total_cost_usd),
            tokens_in => r.tokens_in,
            tokens_out => r.tokens_out,
            requests => r.request_count,
        }
    }).collect();

    // ── Project summary (derived from cost_rows_grouped) ─────────────────────
    let detail_rows = CostRepository::cost_rows_grouped(
        &*state.db, eff_user_ids_ref, filter_project, None, filter_model_opt, &start,
    ).await.map_err(|_| DashboardError::Internal)?;

    let mut project_map: std::collections::BTreeMap<String, (f64, i64, i64, i64)> =
        std::collections::BTreeMap::new();
    for (_, _, proj, _, cost, ti, to, rc) in &detail_rows {
        if let Some(p) = proj {
            let e = project_map.entry(p.clone()).or_insert((0.0, 0, 0, 0));
            e.0 += cost; e.1 += ti; e.2 += to; e.3 += rc;
        }
    }
    let mut by_project_rows: Vec<minijinja::Value> = project_map.iter().map(|(p, (cost, ti, to, rc))| {
        minijinja::context! {
            project => p.clone(),
            cost_usd => format!("{:.4}", cost),
            tokens_in => *ti,
            tokens_out => *to,
            requests => *rc,
        }
    }).collect();
    // sort by cost desc
    by_project_rows.sort_by(|a, b| {
        let ac: f64 = a.get_attr("cost_usd").ok()
            .and_then(|v| v.to_string().parse().ok()).unwrap_or(0.0);
        let bc: f64 = b.get_attr("cost_usd").ok()
            .and_then(|v| v.to_string().parse().ok()).unwrap_or(0.0);
        bc.partial_cmp(&ac).unwrap_or(std::cmp::Ordering::Equal)
    });

    // ── Chart: Top Models (JSON for D3) ───────────────────────────────────────
    let top_models_json = serde_json::to_string(
        &model_rows.iter().map(|r| serde_json::json!({
            "model": r.model,
            "cost": r.total_cost_usd,
        })).collect::<Vec<_>>()
    ).unwrap_or_else(|_| "[]".to_string());

    // ── Chart: Token Usage per user (JSON for D3) ─────────────────────────────
    let token_usage_json = serde_json::to_string(
        &user_stats.iter().map(|(uid, _, ti, to, _)| {
            let name = user_name_map.get(uid).cloned().unwrap_or_else(|| format!("#{}", uid));
            serde_json::json!({ "user": name, "tokens_in": ti, "tokens_out": to })
        }).collect::<Vec<_>>()
    ).unwrap_or_else(|_| "[]".to_string());

    // ── Chart: Burndown (daily cumulative spend + budget limit) ───────────────
    let daily = CostRepository::list_daily_spend(
        &*state.db, eff_user_ids_ref, filter_project, filter_model_opt, &start, &end,
    ).await.map_err(|_| DashboardError::Internal)?;

    let mut cumul = 0.0f64;
    let series: Vec<serde_json::Value> = daily.iter().map(|(date, cost)| {
        cumul += cost;
        serde_json::json!([date, cumul])
    }).collect();

    // Look up budget limit if single user or group is selected and window is monthly
    let budget_limit: Option<f64> = if q.window == "monthly" {
        if let Some(uid) = filter_uid {
            let scope = crate::db::models::BudgetScope::User(uid);
            BudgetRepository::list_for_scope(&*state.db, &scope)
                .await.ok()
                .and_then(|rules| rules.into_iter().find(|r| r.window == "monthly"))
                .and_then(|r| r.limit_usd)
        } else if !q.group.is_empty() {
            let scope = crate::db::models::BudgetScope::Group(q.group.clone());
            BudgetRepository::list_for_scope(&*state.db, &scope)
                .await.ok()
                .and_then(|rules| rules.into_iter().find(|r| r.window == "monthly"))
                .and_then(|r| r.limit_usd)
        } else {
            None
        }
    } else {
        None
    };

    let burndown_json = serde_json::to_string(&serde_json::json!({
        "series": series,
        "limit": budget_limit,
    })).unwrap_or_else(|_| r#"{"series":[],"limit":null}"#.to_string());

    super::dashboard::render("reports_panels.html", minijinja::context! {
        by_user_rows => by_user_rows,
        by_model_rows => by_model_rows,
        by_project_rows => by_project_rows,
        top_models_json => top_models_json,
        token_usage_json => token_usage_json,
        burndown_json => burndown_json,
        window => q.window,
    })
}
