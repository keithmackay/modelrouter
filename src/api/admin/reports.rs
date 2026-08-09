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
    /// Attribution tag key; empty means the correlation id. Paired with `value`.
    #[serde(default)]
    pub key: String,
    /// Attribution value to filter on; empty means "no attribution filter".
    #[serde(default)]
    pub value: String,
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

    // Attribution pickers: tag keys, plus the values available for whichever
    // dimension is selected (correlation ids when no tag key is chosen).
    let attr_keys = CostRepository::distinct_attribution_tag_keys(&*state.db)
        .await.map_err(|_| DashboardError::Internal)?;
    let attr_key_opt = if q.key.is_empty() { None } else { Some(q.key.as_str()) };
    let attr_values = CostRepository::distinct_attribution_values(&*state.db, attr_key_opt, 500)
        .await.map_err(|_| DashboardError::Internal)?;

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
        attr_keys => attr_keys,
        attr_values => attr_values,
        sel_attr_key => q.key,
        sel_attr_value => q.value,
    })
}

pub async fn get_reports_panels(
    State(state): State<AppState>,
    _session: DashboardSession,
    Query(q): Query<ReportsQuery>,
) -> Result<Html<String>, DashboardError> {
    // An attribution filter replaces the panel body: the user/model/project
    // aggregates below cannot express it, and showing both would invite reading
    // unfiltered numbers as if they were attributed.
    if let Some(filter) = attribution_filter(&q)? {
        return attribution_panels(&state, &filter, &q.window).await;
    }

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
            cost_usd => format!("{:.2}", cost),
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
            cost_usd => format!("{:.2}", r.total_cost_usd),
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
            cost_usd => format!("{:.2}", cost),
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

    // ── Chart: Burndown (remaining budget per day) ───────────────────────────
    let daily = CostRepository::list_daily_spend(
        &*state.db, eff_user_ids_ref, filter_project, filter_model_opt, &start, &end,
    ).await.map_err(|_| DashboardError::Internal)?;

    // Look up budget limit — check all windows matching the selected window
    let budget_window = match q.window.as_str() {
        "daily" | "weekly" => q.window.as_str(),
        _ => "monthly",
    };
    let budget_limit: Option<f64> = if let Some(uid) = filter_uid {
        let scope = crate::db::models::BudgetScope::User(uid);
        BudgetRepository::list_for_scope(&*state.db, &scope)
            .await.ok()
            .and_then(|rules| rules.into_iter().find(|r| r.window == budget_window))
            .and_then(|r| r.limit_usd)
    } else if !q.group.is_empty() {
        let scope = crate::db::models::BudgetScope::Group(q.group.clone());
        BudgetRepository::list_for_scope(&*state.db, &scope)
            .await.ok()
            .and_then(|rules| rules.into_iter().find(|r| r.window == budget_window))
            .and_then(|r| r.limit_usd)
    } else {
        // Global budget
        let scope = crate::db::models::BudgetScope::Global;
        BudgetRepository::list_for_scope(&*state.db, &scope)
            .await.ok()
            .and_then(|rules| rules.into_iter().find(|r| r.window == budget_window))
            .and_then(|r| r.limit_usd)
    };

    // Series: raw daily spend values. Frontend computes remaining = limit - cumulative.
    let series: Vec<serde_json::Value> = daily.iter()
        .map(|(date, cost)| serde_json::json!([date, cost]))
        .collect();

    let burndown_json = serde_json::to_string(&serde_json::json!({
        "series": series,
        "limit": budget_limit,
        "start": start.get(..10).unwrap_or(""),
    })).unwrap_or_else(|_| r#"{"series":[],"limit":null,"start":""}"#.to_string());

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

// ── Attribution filter (issue #13) ────────────────────────────────────────────

/// The attribution filter this query selects, if any.
fn attribution_filter(
    q: &ReportsQuery,
) -> Result<Option<crate::db::repositories::costs::AttributionFilter>, DashboardError> {
    let aq = crate::api::admin::attribution::AttributionQuery {
        key: q.key.clone(),
        value: q.value.clone(),
        window: q.window.clone(),
    };
    aq.filter()
        .map_err(|e| DashboardError::BadRequest(e.to_string()))
}

/// Render the attributed-usage panel body.
async fn attribution_panels(
    state: &AppState,
    filter: &crate::db::repositories::costs::AttributionFilter,
    window: &str,
) -> Result<Html<String>, DashboardError> {
    let report = crate::api::admin::attribution::build_report(state, filter, window)
        .await
        .map_err(|_| DashboardError::Internal)?;

    let rows = |src: &[crate::db::repositories::costs::AttributionBreakdownRow]| {
        src.iter()
            .map(|r| {
                minijinja::context! {
                    key => r.key.clone(),
                    cost_usd => format!("{:.4}", r.totals.cost_usd),
                    saved_usd => format!("{:.4}", r.totals.saved_usd),
                    requests => r.totals.requests,
                    cache_hits => r.totals.cache_hits,
                }
            })
            .collect::<Vec<_>>()
    };

    super::dashboard::render(
        "attribution_panels.html",
        minijinja::context! {
            filter_label => report.filter,
            cost_usd => format!("{:.4}", report.totals.cost_usd),
            saved_usd => format!("{:.4}", report.totals.saved_usd),
            requests => report.totals.requests,
            cache_hits => report.totals.cache_hits,
            hit_rate => format!("{:.0}%", report.hit_rate * 100.0),
            tokens_in => report.totals.tokens_in,
            tokens_out => report.totals.tokens_out,
            by_model_rows => rows(&report.by_model),
            by_day_rows => rows(&report.by_day),
        },
    )
}
