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

    /// Record a provider error unless the provider was rejecting the *request*
    /// rather than failing to serve it.
    ///
    /// The breaker is keyed on the provider, so every failure counted against
    /// it is charged to every model behind it. A 400 for an unsupported
    /// parameter is not evidence the provider is unwell — counting it lets one
    /// caller's bad request deny service to unrelated models. This was not
    /// hypothetical: a `temperature` that Anthropic's Claude 5 models no longer
    /// accept 400'd on every skill call, and five of those opened the Vertex
    /// breaker and took down *every* Vertex-backed model, including ones that
    /// would have answered.
    ///
    /// Suppressing the count does not hide the failure: the caller still gets
    /// its error, still gets it fast (client errors are not retried), and it is
    /// still written to the failure log.
    pub fn record_provider_error(&self, provider: &str, err_str: &str) {
        if crate::router::retry::RetryableError::classify(err_str).counts_toward_circuit_breaker() {
            self.record_failure(provider);
        } else {
            tracing::debug!(
                provider,
                error = err_str,
                "client error; not counting toward the provider circuit breaker"
            );
        }
    }

    /// Same policy as [`Self::record_provider_error`], for call sites that hold
    /// the upstream HTTP status directly and need no string parsing.
    pub fn record_upstream_status(&self, provider: &str, status: u16) {
        // 429 is a 4xx but a real health signal — it means this provider cannot
        // take the traffic, which is exactly what the breaker is for.
        let request_fault = (400..500).contains(&status) && status != 429;
        if request_fault {
            tracing::debug!(
                provider,
                status,
                "client error; not counting toward the provider circuit breaker"
            );
        } else {
            self.record_failure(provider);
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
