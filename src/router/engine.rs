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
        // 1. Alias lookup
        if let Some(resolved) = self.settings.routing.model_aliases.get(requested_model) {
            return self.resolve(resolved);
        }
        // 2. Explicit provider prefix "provider/model"
        if let Some(pos) = requested_model.find('/') {
            let provider = requested_model[..pos].to_string();
            let model = requested_model[pos + 1..].to_string();
            return (provider, model);
        }
        // 3. Default provider + default model
        (
            self.settings.routing.default_provider.clone(),
            self.settings.routing.default_model.clone(),
        )
    }
}
