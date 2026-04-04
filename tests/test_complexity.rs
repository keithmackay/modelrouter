use modelrouter::router::complexity::ComplexityRouter;
use modelrouter::config::schema::ComplexityRoutingConfig;
use serde_json::json;

fn config_with_threshold(threshold: u32, cheap_model: &str) -> ComplexityRoutingConfig {
    ComplexityRoutingConfig {
        enabled: true,
        token_threshold: threshold,
        cheap_model: cheap_model.to_string(),
    }
}

#[test]
fn short_messages_stay_on_requested_model() {
    let config = config_with_threshold(100, "gpt-4o-mini");
    let router = ComplexityRouter::new(Some(config));
    let messages = vec![json!({"role": "user", "content": "Hi"})];
    assert_eq!(router.maybe_downgrade("gpt-4o", &messages), "gpt-4o");
}

#[test]
fn long_messages_downgrade_to_cheap_model() {
    let config = config_with_threshold(10, "gpt-4o-mini");
    let router = ComplexityRouter::new(Some(config));
    // "A".repeat(200) → 200 chars / 4 = 50 tokens, > threshold of 10
    let content = "A".repeat(200);
    let messages = vec![json!({"role": "user", "content": content})];
    assert_eq!(router.maybe_downgrade("gpt-4o", &messages), "gpt-4o-mini");
}

#[test]
fn disabled_config_never_downgrades() {
    let config = ComplexityRoutingConfig {
        enabled: false,
        token_threshold: 1,
        cheap_model: "gpt-4o-mini".to_string(),
    };
    let router = ComplexityRouter::new(Some(config));
    let content = "A".repeat(1000);
    let messages = vec![json!({"role": "user", "content": content})];
    assert_eq!(router.maybe_downgrade("gpt-4o", &messages), "gpt-4o");
}

#[test]
fn none_config_never_downgrades() {
    let router = ComplexityRouter::new(None);
    let content = "A".repeat(1000);
    let messages = vec![json!({"role": "user", "content": content})];
    assert_eq!(router.maybe_downgrade("gpt-4o", &messages), "gpt-4o");
}

#[test]
fn multi_message_tokens_summed() {
    let config = config_with_threshold(10, "gpt-4o-mini");
    let router = ComplexityRouter::new(Some(config));
    // Two messages each with 40 chars = 80 chars / 4 = 20 tokens total, > 10
    let messages = vec![
        json!({"role": "user", "content": "A".repeat(40)}),
        json!({"role": "assistant", "content": "B".repeat(40)}),
    ];
    assert_eq!(router.maybe_downgrade("gpt-4o", &messages), "gpt-4o-mini");
}

#[test]
fn estimate_tokens_counts_chars_over_four() {
    // 400 chars → 100 tokens
    assert_eq!(
        modelrouter::router::complexity::estimate_tokens_from_messages(
            &[json!({"role": "user", "content": "A".repeat(400)})]
        ),
        100
    );
}

#[test]
fn exact_threshold_stays_on_requested_model() {
    // threshold=10, content = 40 chars / 4 = exactly 10 tokens — NOT downgraded (strictly >)
    let config = config_with_threshold(10, "gpt-4o-mini");
    let router = ComplexityRouter::new(Some(config));
    let messages = vec![json!({"role": "user", "content": "A".repeat(40)})];
    assert_eq!(router.maybe_downgrade("gpt-4o", &messages), "gpt-4o");
}
