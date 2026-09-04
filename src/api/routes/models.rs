use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::api::{app::AppState, error::ApiError};

/// `GET /v1/models`.
///
/// Lists the providers this router can reach plus every registered model, with
/// anything an operator has disabled omitted (issue #5) — a disabled entity must
/// not advertise itself as available.
pub async fn list_models(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, ApiError> {
    use crate::db::repositories::models::ModelRepository;

    crate::api::routes::completions::reject_experiment_header("/v1/models", &headers)?;
    let availability = state.router.availability();

    let mut models: Vec<Value> = state
        .settings
        .providers
        .keys()
        .filter(|provider| !availability.disabled_providers().contains_key(*provider))
        .map(|provider| {
            json!({
                "id": provider,
                "object": "model",
                "owned_by": provider,
            })
        })
        .collect();

    // Registered models, excluding disabled ones and models on a disabled provider.
    if let Ok(registered) = state.db.list_models().await {
        for m in registered {
            if availability.check(&m.provider, &m.name).is_err() {
                continue;
            }
            models.push(json!({
                "id": format!("{}/{}", m.provider, m.name),
                "object": "model",
                "owned_by": m.provider,
                "alias": m.alias,
            }));
        }
    }

    // Routing aliases (config + DB, issue #25): the names callers actually
    // route with. On a config-alias-only deployment nothing above lists them,
    // so the endpoint used to advertise no models at all. Each alias resolves
    // through the router; unresolvable or disabled targets are omitted, same
    // as disabled registered models.
    let mut seen: std::collections::HashSet<String> = models
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    let mut aliases: Vec<(String, String)> = state.router.alias_map().into_iter().collect();
    aliases.sort();
    for (alias, target) in aliases {
        if seen.contains(&alias) {
            continue;
        }
        let res = state.router.resolve_detailed(&alias);
        if res.substituted {
            continue; // alias chain fell through to default_model — not a real target
        }
        if availability.check(&res.provider, &res.model).is_err() {
            continue;
        }
        seen.insert(alias.clone());
        models.push(json!({
            "id": alias,
            "object": "model",
            "owned_by": res.provider,
            "alias_for": target,
        }));
    }

    Ok(Json(json!({"object": "list", "data": models})))
}
