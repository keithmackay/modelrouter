use modelrouter::config::{schema::RoutingConfig, Settings};
use modelrouter::router::engine::RequestRouter;
use std::collections::HashMap;
use std::sync::Arc;

fn router_with_aliases() -> RequestRouter {
    let mut settings = Settings::default();
    let mut aliases = HashMap::new();
    aliases.insert(
        "fast".to_string(),
        "anthropic/claude-haiku-4-5".to_string(),
    );
    settings.routing = RoutingConfig {
        default_provider: "openai".to_string(),
        default_model: "gpt-4o".to_string(),
        model_aliases: aliases,
        fallback_chains: HashMap::new(),
    };
    RequestRouter::new(Arc::new(settings))
}

#[test]
fn resolve_explicit_provider_prefix() {
    let r = router_with_aliases();
    assert_eq!(
        r.resolve("openai/gpt-4o"),
        ("openai".to_string(), "gpt-4o".to_string())
    );
}

#[test]
fn resolve_alias() {
    let r = router_with_aliases();
    assert_eq!(
        r.resolve("fast"),
        ("anthropic".to_string(), "claude-haiku-4-5".to_string())
    );
}

#[test]
fn resolve_default() {
    let r = router_with_aliases();
    assert_eq!(
        r.resolve("unknown-model"),
        ("openai".to_string(), "gpt-4o".to_string())
    );
}
