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
        complexity_routing: None,
        load_balancer: HashMap::new(),
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

#[test]
fn test_fallback_chain_next_model() {
    use modelrouter::router::fallback::FallbackChain;
    use std::collections::HashMap;

    let mut chains = HashMap::new();
    chains.insert(
        "gpt-4o".to_string(),
        vec!["gpt-4o-mini".to_string(), "gpt-3.5-turbo".to_string()],
    );

    let chain = FallbackChain::new(chains);

    assert_eq!(chain.next_after("gpt-4o"), Some("gpt-4o-mini"));
    assert_eq!(chain.next_after("gpt-4o-mini"), Some("gpt-3.5-turbo"));
    assert_eq!(chain.next_after("gpt-3.5-turbo"), None);
    assert_eq!(chain.next_after("unknown-model"), None);
}

#[test]
fn test_fallback_chain_empty() {
    use modelrouter::router::fallback::FallbackChain;
    use std::collections::HashMap;

    let chain = FallbackChain::new(HashMap::new());
    assert_eq!(chain.next_after("gpt-4o"), None);
}
