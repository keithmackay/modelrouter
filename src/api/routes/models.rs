use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::api::app::AppState;

/// `GET /v1/models`.
///
/// Lists the providers this router can reach plus every registered model, with
/// anything an operator has disabled omitted (issue #5) — a disabled entity must
/// not advertise itself as available.
pub async fn list_models(State(state): State<AppState>) -> Json<Value> {
    use crate::db::repositories::models::ModelRepository;

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

    Json(json!({"object": "list", "data": models}))
}
