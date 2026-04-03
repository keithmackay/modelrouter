use axum::{extract::State, response::IntoResponse};
use crate::api::app::AppState;

pub async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    #[cfg(feature = "prometheus")]
    {
        use ::prometheus::Encoder;
        if let Some(ref metrics) = state.app_metrics {
            let encoder = ::prometheus::TextEncoder::new();
            let families = metrics.registry.gather();
            let mut buf = Vec::new();
            encoder.encode(&families, &mut buf).unwrap_or_default();
            return (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
                buf,
            ).into_response();
        }
    }
    axum::http::StatusCode::NOT_FOUND.into_response()
}
