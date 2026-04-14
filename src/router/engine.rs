use std::sync::Arc;

use crate::config::Settings;

pub struct RequestRouter {
    settings: Arc<Settings>,
}

impl RequestRouter {
    pub fn new(settings: Arc<Settings>) -> Self {
        Self { settings }
    }

    pub fn resolve(&self, requested_model: &str) -> (String, String) {
        let mut current = requested_model.to_string();
        let mut depth = 0;
        const MAX_ALIAS_DEPTH: usize = 10;

        while depth < MAX_ALIAS_DEPTH {
            // 1. Alias lookup
            if let Some(resolved) = self.settings.routing.model_aliases.get(&current) {
                current = resolved.clone();
                depth += 1;
                continue;
            }
            // 2. Explicit provider prefix "provider/model"
            if let Some(pos) = current.find('/') {
                let provider = current[..pos].to_string();
                let model = current[pos + 1..].to_string();
                return (provider, model);
            }
            // Not an alias, not a prefix — break to fallback
            break;
        }

        // If we ended up with a "provider/model" form after alias resolution
        if let Some(pos) = current.find('/') {
            return (current[..pos].to_string(), current[pos + 1..].to_string());
        }

        // 3. Default provider + default model.
        // default_model may be "provider/model" — strip the prefix so callers
        // always receive a bare model name as the second element of the tuple.
        let default = &self.settings.routing.default_model;
        if let Some(pos) = default.find('/') {
            (default[..pos].to_string(), default[pos + 1..].to_string())
        } else {
            (self.settings.routing.default_provider.clone(), default.clone())
        }
    }
}
