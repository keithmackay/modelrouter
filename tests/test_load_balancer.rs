use modelrouter::router::load_balancer::{LoadBalancer, LoadBalancerConfig, LbPoolEntry, LbStrategy};
use std::collections::HashMap;

fn round_robin_pool(entries: Vec<(&str, &str, u32)>) -> LoadBalancerConfig {
    LoadBalancerConfig {
        strategy: LbStrategy::RoundRobin,
        pool: entries
            .into_iter()
            .map(|(provider, model, weight)| LbPoolEntry {
                provider: provider.to_string(),
                model: model.to_string(),
                weight,
            })
            .collect(),
    }
}

fn weighted_pool(entries: Vec<(&str, &str, u32)>) -> LoadBalancerConfig {
    LoadBalancerConfig {
        strategy: LbStrategy::Weighted,
        pool: entries
            .into_iter()
            .map(|(provider, model, weight)| LbPoolEntry {
                provider: provider.to_string(),
                model: model.to_string(),
                weight,
            })
            .collect(),
    }
}

#[test]
fn round_robin_cycles_through_all_entries() {
    let mut pools = HashMap::new();
    pools.insert(
        "my-pool".to_string(),
        round_robin_pool(vec![
            ("openai", "gpt-4o", 1),
            ("anthropic", "claude-opus-4-5", 1),
        ]),
    );
    let lb = LoadBalancer::new(pools);

    let (p1, _) = lb.resolve("my-pool").unwrap();
    let (p2, _) = lb.resolve("my-pool").unwrap();
    let (p3, _) = lb.resolve("my-pool").unwrap();

    // First two are different
    assert_ne!(p1, p2);
    // Third wraps around to first
    assert_eq!(p1, p3);
}

#[test]
fn unknown_model_returns_none() {
    let lb = LoadBalancer::new(HashMap::new());
    assert!(lb.resolve("gpt-4o").is_none());
}

#[test]
fn single_entry_pool_always_returns_same() {
    let mut pools = HashMap::new();
    pools.insert(
        "single".to_string(),
        round_robin_pool(vec![("openai", "gpt-4o", 1)]),
    );
    let lb = LoadBalancer::new(pools);
    let first = lb.resolve("single").unwrap();
    let second = lb.resolve("single").unwrap();
    assert_eq!(first, second);
}

#[test]
fn weighted_distributes_proportionally() {
    let mut pools = HashMap::new();
    pools.insert(
        "weighted".to_string(),
        weighted_pool(vec![
            ("openai", "gpt-4o", 2),
            ("anthropic", "claude-opus-4-5", 1),
        ]),
    );
    let lb = LoadBalancer::new(pools);

    // Cycle through 3 calls — with weights 2:1, expanded = [openai, openai, anthropic]
    let results: Vec<_> = (0..3).map(|_| lb.resolve("weighted").unwrap().0).collect();
    let openai_count = results.iter().filter(|p| p.as_str() == "openai").count();
    let anthropic_count = results.iter().filter(|p| p.as_str() == "anthropic").count();
    assert_eq!(openai_count, 2);
    assert_eq!(anthropic_count, 1);
}

#[test]
fn empty_pool_returns_none() {
    let mut pools = HashMap::new();
    pools.insert(
        "empty".to_string(),
        LoadBalancerConfig {
            strategy: LbStrategy::RoundRobin,
            pool: vec![],
        },
    );
    let lb = LoadBalancer::new(pools);
    assert!(lb.resolve("empty").is_none());
}
