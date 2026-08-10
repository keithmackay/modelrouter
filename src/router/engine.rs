use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use crate::config::Settings;

/// The outcome of resolving a requested model name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub provider: String,
    pub model: String,
    /// True when nothing matched and `default_model` was substituted for what
    /// the caller actually asked for.
    pub substituted: bool,
}

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

    /// Resolve a requested model, reporting whether the answer is a SUBSTITUTION.
    ///
    /// `substituted` is true when the requested name matched no alias, carried no
    /// `provider/` prefix, and therefore fell through to `default_model`. That case
    /// used to be indistinguishable from a real match, and it is not a harmless
    /// default: the caller asked for one model and a different one answered, with a
    /// successful-looking prompt row to show for it. Callers must either reject it
    /// (`routing.strict_model_resolution`) or say so in the logs.
    pub fn resolve_detailed(&self, requested_model: &str) -> Resolution {
        let (provider, model, substituted) = self.resolve_inner(requested_model);
        Resolution { provider, model, substituted }
    }

    pub fn resolve(&self, requested_model: &str) -> (String, String) {
        let (provider, model, _) = self.resolve_inner(requested_model);
        (provider, model)
    }

    fn resolve_inner(&self, requested_model: &str) -> (String, String, bool) {
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
                return (provider, model, false);
            }
            // Not an alias, not a prefix — break to fallback
            break;
        }

        // If we ended up with a "provider/model" form after alias resolution
        if let Some(pos) = current.find('/') {
            return (current[..pos].to_string(), current[pos + 1..].to_string(), false);
        }

        // 4. Nothing matched — fall through to the default. This is a SUBSTITUTION:
        // the caller asked for `requested_model` and something else will answer.
        let default = &self.settings.routing.default_model;
        if let Some(pos) = default.find('/') {
            (default[..pos].to_string(), default[pos + 1..].to_string(), true)
        } else {
            (self.settings.routing.default_provider.clone(), default.clone(), true)
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

    /// The live case this flag exists for: `claude-opus-4-5-20251101` is neither an
    /// alias nor `provider/model`, so it falls through to the default and is answered
    /// by `gpt-4o-mini`. That happened 1,330 times without a single signal, because
    /// the substitution was indistinguishable from a match.
    #[test]
    fn an_unmatched_bare_model_name_reports_itself_as_substituted() {
        let r = router_with_shortcuts(None, None);
        let res = r.resolve_detailed("claude-opus-4-5-20251101");
        assert!(
            res.substituted,
            "falling through to default_model must be reported as a substitution"
        );
        assert_eq!(res.provider, "openai");
        assert_eq!(res.model, "gpt-4o");
    }

    #[test]
    fn an_explicit_provider_prefix_is_not_a_substitution() {
        let r = router_with_shortcuts(None, None);
        let res = r.resolve_detailed("anthropic/claude-opus-4-5");
        assert!(!res.substituted);
        assert_eq!(res.provider, "anthropic");
    }

    #[test]
    fn a_configured_alias_is_not_a_substitution() {
        let mut s = Settings::default();
        s.routing
            .model_aliases
            .insert("fast".to_string(), "openai/gpt-4o-mini".to_string());
        let r = RequestRouter::new(Arc::new(s));
        let res = r.resolve_detailed("fast");
        assert!(!res.substituted);
        assert_eq!(res.provider, "openai");
        assert_eq!(res.model, "gpt-4o-mini");
    }

    #[test]
    fn normal_model_unaffected_by_shortcuts() {
        let r = router_with_shortcuts(Some("x/y"), Some("a/b"));
        let (provider, model) = r.resolve("anthropic/claude-opus-4-5");
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-opus-4-5");
    }
}
