use modelrouter::router::cost::CostCalculator;

#[test]
fn cost_calculation_gpt4o() {
    let calc = CostCalculator::new();
    // 1000 prompt tokens + 500 completion tokens with gpt-4o pricing
    let cost = calc.calculate("gpt-4o", 1000, 500);
    // input: 1000/1M * 2.50 = 0.0025, output: 500/1M * 10.0 = 0.005
    assert!(
        (cost - 0.0075).abs() < 0.0001,
        "Expected ~0.0075, got {}",
        cost
    );
}

#[test]
fn cost_calculation_unknown_model_returns_zero() {
    let calc = CostCalculator::new();
    assert_eq!(calc.calculate("ollama/llama3", 1000, 500), 0.0);
}

#[test]
fn cost_calculation_strips_provider_prefix() {
    let calc = CostCalculator::new();
    let with_prefix = calc.calculate("anthropic/claude-haiku-4-5", 1000, 500);
    let without_prefix = calc.calculate("claude-haiku-4-5", 1000, 500);
    assert_eq!(with_prefix, without_prefix);
    assert!(with_prefix > 0.0);
}
