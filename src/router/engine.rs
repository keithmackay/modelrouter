use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use crate::config::Settings;
use crate::router::availability::{AvailabilityMap, Unavailable};

pub struct RequestRouter {
    settings: Arc<Settings>,
    /// DB-sourced alias overrides. DB wins over config on conflict.
    db_aliases: Arc<ArcSwap<HashMap<String, String>>>,
    /// Models and providers an operator has taken out of rotation (issue #5).
    availability: Arc<ArcSwap<AvailabilityMap>>,
}

impl RequestRouter {
    pub fn new(settings: Arc<Settings>) -> Self {
        Self {
            settings,
            db_aliases: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            availability: Arc::new(ArcSwap::from_pointee(AvailabilityMap::default())),
        }
    }

    /// Replace the live DB alias map (called after DB model writes).
    pub fn update_db_aliases(&self, aliases: HashMap<String, String>) {
        self.db_aliases.store(Arc::new(aliases));
    }

    /// Replace the live operator-disable snapshot (called after any enable/disable write).
    pub fn update_availability(&self, map: AvailabilityMap) {
        self.availability.store(Arc::new(map));
    }

    /// Current operator-disable snapshot.
    pub fn availability(&self) -> Arc<AvailabilityMap> {
        self.availability.load_full()
    }

    /// `Err` when an operator has disabled this model or its provider.
    ///
    /// Distinct from a circuit-breaker trip: this is sticky and only an explicit
    /// re-enable clears it. See [`crate::router::availability`].
    pub fn check_available(&self, provider: &str, model: &str) -> Result<(), Unavailable> {
        self.availability.load().check(provider, model)
    }

    pub fn is_available(&self, provider: &str, model: &str) -> bool {
        self.check_available(provider, model).is_ok()
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

    fn db_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn db_alias_beats_config_alias() {
        let mut s = Settings::default();
        s.routing
            .model_aliases
            .insert("deep".to_string(), "openai/gpt-4o".to_string());
        let r = RequestRouter::new(Arc::new(s));
        // Config alias applies before any DB alias is installed.
        assert_eq!(r.resolve("deep"), ("openai".to_string(), "gpt-4o".to_string()));

        // Installing a DB alias takes precedence, with no restart.
        r.update_db_aliases(db_map(&[("deep", "anthropic/claude-opus-4-6")]));
        assert_eq!(
            r.resolve("deep"),
            ("anthropic".to_string(), "claude-opus-4-6".to_string())
        );

        // Removing it again falls back to the config alias.
        r.update_db_aliases(HashMap::new());
        assert_eq!(r.resolve("deep"), ("openai".to_string(), "gpt-4o".to_string()));
    }

    #[test]
    fn alias_chain_resolves_through_multiple_hops() {
        let r = RequestRouter::new(Arc::new(Settings::default()));
        r.update_db_aliases(db_map(&[
            ("deep", "premium"),
            ("premium", "anthropic/claude-opus-4-6"),
        ]));
        assert_eq!(
            r.resolve("deep"),
            ("anthropic".to_string(), "claude-opus-4-6".to_string())
        );
    }

    #[test]
    fn alias_cycle_is_rejected_by_the_depth_cap() {
        // Even if a cycle reaches the router (e.g. written directly to the DB),
        // MAX_ALIAS_DEPTH must bound resolution and fall through to the default.
        let r = RequestRouter::new(Arc::new(Settings::default()));
        r.update_db_aliases(db_map(&[("a", "b"), ("b", "a")]));
        let (provider, model) = r.resolve("a");
        assert_eq!(provider, "openai"); // default_provider — not a hang, not "a"/"b"
        assert_ne!(model, "a");
        assert_ne!(model, "b");
    }

    #[test]
    fn self_referential_alias_terminates() {
        let r = RequestRouter::new(Arc::new(Settings::default()));
        r.update_db_aliases(db_map(&[("loop", "loop")]));
        let (provider, _) = r.resolve("loop");
        assert_eq!(provider, "openai");
    }

    #[test]
    fn shortcuts_cannot_be_shadowed_by_db_aliases() {
        let r = router_with_shortcuts(Some("anthropic/claude-haiku-4-5"), None);
        r.update_db_aliases(db_map(&[(":fastest", "evil/model")]));
        let (provider, model) = r.resolve(":fastest");
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-haiku-4-5");
    }

    #[test]
    fn normal_model_unaffected_by_shortcuts() {
        let r = router_with_shortcuts(Some("x/y"), Some("a/b"));
        let (provider, model) = r.resolve("anthropic/claude-opus-4-5");
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-opus-4-5");
    }
}
