use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use crate::config::Settings;

pub struct RequestRouter {
    settings: Arc<Settings>,
    /// DB-sourced alias overrides. DB wins over config on conflict.
    db_aliases: Arc<ArcSwap<HashMap<String, String>>>,
}

impl RequestRouter {
    pub fn new(settings: Arc<Settings>) -> Self {
        Self {
            settings,
            db_aliases: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        }
    }

    /// Replace the live DB alias map (called after DB model writes).
    pub fn update_db_aliases(&self, aliases: HashMap<String, String>) {
        self.db_aliases.store(Arc::new(aliases));
    }

    pub fn resolve(&self, requested_model: &str) -> (String, String) {
        let db_map = self.db_aliases.load();
        let mut current = requested_model.to_string();
        let mut depth = 0;
        const MAX_ALIAS_DEPTH: usize = 10;

        while depth < MAX_ALIAS_DEPTH {
            // 1. DB alias lookup (takes priority over config)
            if let Some(resolved) = db_map.get(&current) {
                current = resolved.clone();
                depth += 1;
                continue;
            }
            // 2. Config alias lookup
            if let Some(resolved) = self.settings.routing.model_aliases.get(&current) {
                current = resolved.clone();
                depth += 1;
                continue;
            }
            // 3. Explicit provider prefix "provider/model"
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

        // 4. Default provider + default model.
        let default = &self.settings.routing.default_model;
        if let Some(pos) = default.find('/') {
            (default[..pos].to_string(), default[pos + 1..].to_string())
        } else {
            (self.settings.routing.default_provider.clone(), default.clone())
        }
    }
}
