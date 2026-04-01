use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::api::app::AppState;

pub async fn list_models(State(state): State<AppState>) -> Json<Value> {
    let models: Vec<Value> = state
        .settings
        .providers
        .keys()
        .map(|provider| {
            json!({
                "id": provider,
                "object": "model",
                "owned_by": provider,
            })
        })
        .collect();
    Json(json!({"object": "list", "data": models}))
}
