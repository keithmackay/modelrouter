// tests/test_circuit_breaker.rs
use modelrouter::router::circuit_breaker::CircuitBreaker;

#[test]
fn new_circuit_starts_closed() {
    let cb = CircuitBreaker::new(3, 60);
    assert!(!cb.is_open("openai"), "new circuit must be closed");
}

#[test]
fn circuit_opens_after_threshold_failures() {
    let cb = CircuitBreaker::new(3, 60);
    cb.record_failure("openai");
    cb.record_failure("openai");
    assert!(!cb.is_open("openai"), "still closed before threshold");
    cb.record_failure("openai");
    assert!(cb.is_open("openai"), "must be open after 3 failures");
}

#[test]
fn success_resets_failure_count() {
    let cb = CircuitBreaker::new(3, 60);
    cb.record_failure("openai");
    cb.record_failure("openai");
    cb.record_success("openai");
    cb.record_failure("openai");
    cb.record_failure("openai");
    assert!(!cb.is_open("openai"), "counter resets on success, 2 failures not enough");
}

#[test]
fn open_circuit_transitions_to_half_open_after_zero_cooldown() {
    let cb = CircuitBreaker::new(1, 0);
    cb.record_failure("openai");
    assert!(cb.is_open("openai"), "open after 1 failure");
    assert!(!cb.is_open("openai"), "half-open after cooldown elapsed");
}

#[test]
fn half_open_closes_on_success() {
    let cb = CircuitBreaker::new(1, 0);
    cb.record_failure("openai");
    assert!(cb.is_open("openai"));
    assert!(!cb.is_open("openai")); // half-open
    cb.record_success("openai");
    assert!(!cb.is_open("openai"), "closed after success in half-open");
}

#[test]
fn half_open_reopens_on_failure() {
    let cb = CircuitBreaker::new(1, 0);
    cb.record_failure("openai");
    assert!(cb.is_open("openai"));
    assert!(!cb.is_open("openai")); // half-open
    cb.record_failure("openai");
    assert!(cb.is_open("openai"), "re-opened after failure in half-open");
}

#[test]
fn providers_tracked_independently() {
    let cb = CircuitBreaker::new(2, 60);
    cb.record_failure("openai");
    cb.record_failure("openai");
    assert!(cb.is_open("openai"), "openai open");
    assert!(!cb.is_open("anthropic"), "anthropic unaffected");
}
