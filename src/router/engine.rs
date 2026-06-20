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
        // Shortcut keywords — resolved first so they cannot be shadowed by user aliases
        let after_shortcut = match requested_model {
            ":fastest" => self.settings.routing.shortcuts.fastest
                .as_deref()
                .unwrap_or(requested_model),
            ":cheapest" => self.settings.routing.shortcuts.cheapest
                .as_deref()
                .unwrap_or(requested_model),
            other => other,
        };
        let mut current = after_shortcut.to_string();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::config::schema::{Settings, RoutingShortcutsConfig};

    fn router_with_shortcuts(fastest: Option<&str>, cheapest: Option<&str>) -> RequestRouter {
        let mut s = Settings::default();
        s.routing.shortcuts = RoutingShortcutsConfig {
            fastest: fastest.map(str::to_string),
            cheapest: cheapest.map(str::to_string),
        };
        RequestRouter::new(Arc::new(s))
    }

    #[test]
    fn fastest_resolves_configured_model() {
        let r = router_with_shortcuts(Some("anthropic/claude-haiku-4-5"), None);
        let (provider, model) = r.resolve(":fastest");
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-haiku-4-5");
    }

    #[test]
    fn cheapest_resolves_configured_model() {
        let r = router_with_shortcuts(None, Some("deepseek/deepseek-chat"));
        let (provider, model) = r.resolve(":cheapest");
        assert_eq!(provider, "deepseek");
        assert_eq!(model, "deepseek-chat");
    }

    #[test]
    fn shortcut_not_configured_falls_through() {
        let r = router_with_shortcuts(None, None);
        // Without config, :fastest resolves like any unknown model → default
        let (provider, _) = r.resolve(":fastest");
        assert_eq!(provider, "openai"); // default_provider
    }

    #[test]
    fn normal_model_unaffected_by_shortcuts() {
        let r = router_with_shortcuts(Some("x/y"), Some("a/b"));
        let (provider, model) = r.resolve("anthropic/claude-opus-4-5");
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-opus-4-5");
    }
}
