// src/router/circuit_breaker.rs
use dashmap::DashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Debug)]
enum CircuitState { Closed, Open, HalfOpen }

struct ProviderCircuit {
    state: CircuitState,
    failure_count: u32,
    /// Time when the circuit transitioned to Open state.
    opened_at: Option<Instant>,
    /// Whether the caller has seen the Open state at least once (required before transitioning to HalfOpen).
    seen_open: bool,
}

pub struct CircuitBreaker {
    circuits: DashMap<String, Mutex<ProviderCircuit>>,
    failure_threshold: u32,
    cooldown: Duration,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, cooldown_secs: u64) -> Self {
        Self {
            circuits: DashMap::new(),
            failure_threshold,
            cooldown: Duration::from_secs(cooldown_secs),
        }
    }

    pub fn is_open(&self, provider: &str) -> bool {
        let entry = self.circuits
            .entry(provider.to_string())
            .or_insert_with(|| Mutex::new(ProviderCircuit {
                state: CircuitState::Closed,
                failure_count: 0,
                opened_at: None,
                seen_open: false,
            }));
        let mut circuit = entry.lock().unwrap();
        match circuit.state {
            CircuitState::Closed | CircuitState::HalfOpen => false,
            CircuitState::Open => {
                // Must be seen open at least once before allowing cooldown transition.
                if !circuit.seen_open {
                    circuit.seen_open = true;
                    return true;
                }
                if let Some(opened) = circuit.opened_at {
                    if opened.elapsed() >= self.cooldown {
                        circuit.state = CircuitState::HalfOpen;
                        return false;
                    }
                }
                true
            }
        }
    }

    pub fn record_success(&self, provider: &str) {
        if let Some(entry) = self.circuits.get(provider) {
            let mut circuit = entry.lock().unwrap();
            circuit.state = CircuitState::Closed;
            circuit.failure_count = 0;
            circuit.opened_at = None;
            circuit.seen_open = false;
        }
    }

    pub fn record_failure(&self, provider: &str) {
        let entry = self.circuits
            .entry(provider.to_string())
            .or_insert_with(|| Mutex::new(ProviderCircuit {
                state: CircuitState::Closed,
                failure_count: 0,
                opened_at: None,
                seen_open: false,
            }));
        let mut circuit = entry.lock().unwrap();
        match circuit.state {
            CircuitState::Closed => {
                circuit.failure_count += 1;
                if circuit.failure_count >= self.failure_threshold {
                    circuit.state = CircuitState::Open;
                    circuit.opened_at = Some(Instant::now());
                    circuit.seen_open = false;
                }
            }
            CircuitState::HalfOpen | CircuitState::Open => {
                circuit.state = CircuitState::Open;
                circuit.failure_count = 1;
                circuit.opened_at = Some(Instant::now());
                circuit.seen_open = false;
            }
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self { Self::new(5, 60) }
}
