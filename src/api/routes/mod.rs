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

use crate::api::{app::AppState, error::ApiError};

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
