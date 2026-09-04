pub mod audio;
pub mod completions;
pub mod mcp;
pub mod images;
pub mod responses;
pub mod embeddings;
pub mod feedback;
pub mod health;
pub mod messages;
pub mod models;
pub mod prometheus;
pub mod search;

use crate::{
    api::{app::AppState, error::ApiError},
    router::experiments::BindError,
};

/// Log a refused experiment header under one stable event name, then hand the
/// error on to become the 400. Every refusal — a bind that failed here or the
/// header on an endpoint that does not run experiments — goes through this so
/// rejections can be counted from the logs without any metric plumbing.
pub(crate) fn experiment_bind_rejected(endpoint: &'static str, err: BindError) -> ApiError {
    tracing::warn!(endpoint, error = %err, "experiment_bind_rejected");
    ApiError::from(err)
}

/// Refuse `x-modelrouter-experiment` on an endpoint that does not run
/// experiments. Called first thing by every `/v1` handler other than chat
/// completions, so a caller who sets the header by mistake gets a 400 rather
/// than unmarked traffic.
pub fn reject_experiment_header(
    endpoint: &'static str,
    headers: &axum::http::HeaderMap,
) -> Result<(), ApiError> {
    crate::router::experiments::reject_header(headers)
        .map_err(|e| experiment_bind_rejected(endpoint, e))
}

/// Reject or announce a model SUBSTITUTION.
///
/// When a requested model matches no alias and carries no `provider/` prefix, the
/// router answers with `default_model` instead. That silent swap produced 1,330
/// requests answered by `gpt-4o-mini` while the caller believed it was using Opus —
/// all recorded as ordinary successes, because nothing distinguished a substitution
/// from a match.
///
/// With `routing.strict_model_resolution` on, the request is refused and names the
/// model. With it off (the default, preserving historical behaviour) the swap still
/// happens but is announced at WARN with both model names, so it is greppable
/// instead of invisible.
pub fn guard_model_substitution(
    state: &AppState,
    requested_model: &str,
) -> Result<(), ApiError> {
    let resolution = state.router.resolve_detailed(requested_model);
    if !resolution.substituted {
        return Ok(());
    }

    if state.settings.routing.strict_model_resolution {
        return Err(ApiError::InvalidRequest(format!(
            "model '{}' is not a configured alias and names no provider; \
             refusing to substitute '{}' for it. Add it to [routing.model_aliases], \
             request it as 'provider/model', or disable routing.strict_model_resolution.",
            requested_model, resolution.model
        )));
    }

    tracing::warn!(
        requested_model,
        substituted_model = resolution.model.as_str(),
        substituted_provider = resolution.provider.as_str(),
        "model not configured — substituting the default; the caller will be told \
         it got what it asked for. Set routing.strict_model_resolution to refuse instead."
    );
    Ok(())
}
