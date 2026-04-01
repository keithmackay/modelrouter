# modelrouter Phase 9 — OpenTelemetry Integration

_Written: 2026-04-01_

---

## Overview

Add full OpenTelemetry (OTel) observability to modelrouter: distributed traces, metrics, and structured logs forwarded via OTLP/gRPC to an OpenTelemetry Collector (or any OTLP-compatible backend). The feature is opt-in via `--features otel` and disabled at runtime when no endpoint is configured.

**Signals:** Traces + Metrics + Logs (all three OTel signal types)
**Transport:** OTLP gRPC (`opentelemetry-otlp` with `tonic`)
**Sampling:** Smart — errors and force-sampled spans always recorded; normal requests sampled at a configurable ratio
**Feature flag:** `--features otel` (same pattern as `--features postgres`)

---

## Configuration

New optional `[telemetry]` section in `config.example.toml` and `TelemetryConfig` struct in `src/config/schema.rs`. The struct and all config parsing is gated behind `#[cfg(feature = "otel")]`.

```toml
[telemetry]
enabled = true
endpoint = "http://localhost:4317"   # OTLP gRPC endpoint
service_name = "modelrouter"
sample_ratio = 0.1                   # fraction of normal requests to trace (0.0–1.0)
slow_threshold_ms = 2000             # requests above this are always traced
batch_queue_size = 2048
batch_scheduled_delay_ms = 5000
batch_max_export_size = 512
```

All fields have defaults. If `enabled = false` or the `[telemetry]` block is absent, the OTel pipelines are not started and the binary behaves as if the feature were off.

---

## Architecture

### Module structure

```
src/telemetry/           (new, entirely #[cfg(feature = "otel")])
├── mod.rs               — init_telemetry(), TelemetryShutdownGuard
├── sampler.rs           — SmartSampler implementing opentelemetry_sdk::trace::Sampler
└── metrics.rs           — Meter wrapper, all instrument definitions as Arc<T> statics
```

### Three signal pipelines

| Signal | Processor | Transport |
|--------|-----------|-----------|
| Traces | `BatchSpanProcessor` | OTLP gRPC (tonic) |
| Metrics | `PeriodicReader` (15s flush) | OTLP gRPC (tonic) |
| Logs | `BatchLogProcessor` | OTLP gRPC (tonic) |

All three share the same gRPC endpoint. Exporters are constructed once at startup and registered as global providers.

### Subscriber stack

The existing `tracing_subscriber` init (currently absent — this phase adds it) becomes a layered stack:

```
tracing_subscriber::Registry
  + EnvFilter              (RUST_LOG / config)
  + fmt::Layer             (stdout, existing behavior)
  + OpenTelemetryLayer     (traces bridge, cfg(feature = "otel"))
  + OpenTelemetryBridge    (logs bridge, cfg(feature = "otel"))
```

The `tracing-opentelemetry` crate bridges `tracing` spans into OTel trace spans. The `opentelemetry-appender-tracing` crate bridges `tracing` log events into OTel log records. Both are no-ops at zero cost when the feature is off.

### Global access pattern

The tracer and meter are set as global providers at init time and accessed via `opentelemetry::global::tracer("modelrouter")` / `global::meter("modelrouter")`. No changes to `AppState` or function signatures are required.

### Initialization sequence

In `src/cli/mod.rs` `Commands::Serve`, before `build_router()`:

1. Call `init_telemetry(&settings.telemetry)` — builds all three pipelines, installs global providers, returns `TelemetryShutdownGuard`
2. Build `AppState` (unchanged)
3. Start axum server
4. On graceful shutdown: `TelemetryShutdownGuard::drop()` flushes all in-flight spans, metrics, and logs before the process exits

---

## Instrumentation Points

### Layer 1 — HTTP boundary (`src/api/app.rs`)

Wire up the already-present `tower-http` `TraceLayer` on the axum router. This is done **unconditionally** (not cfg-gated) since `tower-http` is already an unconditional dependency — the layer is zero-overhead when no tracing subscriber is installed. Produces a root span per HTTP request with: `http.method`, `http.route`, `http.status_code`, `http.user_agent`.

### Layer 2 — Completions handler (`src/api/routes/completions.rs`)

`#[instrument(skip(state, req))]` on `chat_completions`. Span attributes added after resolution:

- `user.id`, `model.requested`, `model.canonical`, `provider.name`
- `tokens.prompt`, `tokens.completion`, `cost.usd`, `latency_ms`
- `streaming: bool`
- `modelrouter.force_sample: bool` — set `true` on budget-exceeded denials and errors

### Layer 3 — Policy + provider call (child spans)

Short-lived child spans around the two highest-variance operations inside the completions handler:

- `modelrouter.policy_check` — wraps `PolicyEngine::check()`; attributes: `policy.result` (allow/deny), `policy.reason`
- `modelrouter.provider_call` — wraps the provider adapter dispatch; attributes: `provider.name`, `http.status_code` from the upstream response

### Layer 4 — Hooks (`src/hooks/pipeline.rs`, `src/hooks/lifecycle.rs`)

`#[instrument]` on hook execution functions. Attributes: `hook.name`, `hook.type` (lifecycle/pipeline), `hook.duration_ms`, `hook.success`.

**Not instrumented:** DB writes (fire-and-forget, not on critical path), admin API routes (latency-insensitive, covered by Layer 1 HTTP span).

---

## Metrics Instruments

Defined in `src/telemetry/metrics.rs`. All instruments are created once at startup and held in `OnceLock<T>` (or equivalent) — `Counter<T>` and `Histogram<T>` are not `Sync` in all SDK versions and cannot be `static` items directly. Recording a metric in the hot path is a single atomic operation with no allocation.

| Instrument | Type | Labels |
|---|---|---|
| `modelrouter.requests.total` | Counter (u64) | `model`, `provider`, `status` |
| `modelrouter.tokens.prompt` | Counter (u64) | `model`, `provider` |
| `modelrouter.tokens.completion` | Counter (u64) | `model`, `provider` |
| `modelrouter.cost.usd` | Counter (f64) | `model`, `provider`, `user_id` |
| `modelrouter.request.duration_ms` | Histogram (f64) | `model`, `provider`, `streaming` |
| `modelrouter.policy.denied` | Counter (u64) | `reason` |
| `modelrouter.hooks.duration_ms` | Histogram (f64) | `hook_name`, `hook_type` |

`status` values: `ok`, `error`, `policy_denied`
`reason` values: `budget`, `rate_limit`, `model_denied`

**Deferred:** `budget.utilization` gauge (requires DB callback on every collection cycle — adds DB coupling to the metrics pipeline; post-v0.1.0).

---

## Smart Sampler

`SmartSampler` in `src/telemetry/sampler.rs` implements `opentelemetry_sdk::trace::Sampler`:

```
is parent span already sampled?          → RECORD_AND_SAMPLE
does span have force_sample = true?      → RECORD_AND_SAMPLE
random(0.0..1.0) < config.sample_ratio? → RECORD_AND_SAMPLE
                                         → DROP
```

**Head-based sampling note:** `should_sample()` is called at span *start* before any status or duration is set. Span status (`Error`) and latency are therefore unavailable to the sampler. Both error detection and slow-request detection are handled via the `modelrouter.force_sample = true` attribute, which callers set *before the span closes* when they detect an error or when elapsed time exceeds `slow_threshold_ms`. The sampler checks only this attribute — it does not inspect span status directly.

**Force-sample escape hatch:** Any code path can call `span.set_attribute("modelrouter.force_sample", true)` to guarantee the span is recorded, regardless of sample ratio. Used for budget-exceeded denials, authentication failures, and provider errors.

---

## Shutdown & Graceful Flush

`TelemetryShutdownGuard` holds explicit handles to all three pipeline processors (tracer provider, `SdkMeterProvider`, logger provider). Its `Drop` implementation calls:

1. `opentelemetry::global::shutdown_tracer_provider()`
2. `opentelemetry::global::shutdown_logger_provider()`
3. `sdk_meter_provider.force_flush()` then `sdk_meter_provider.shutdown()` — called on the held `SdkMeterProvider` handle directly, since there is no `global::shutdown_meter_provider()` API in opentelemetry 0.27

This ensures in-flight telemetry is exported before the process exits — essential for short-lived invocations and `systemctl stop` / Ctrl-C scenarios.

---

## Dependencies (feature-gated)

```toml
[features]
otel = [
  "dep:opentelemetry",
  "dep:opentelemetry_sdk",
  "dep:opentelemetry-otlp",
  "dep:opentelemetry-appender-tracing",
  "dep:tracing-opentelemetry",
]

[dependencies]
opentelemetry           = { version = "0.27", optional = true }
opentelemetry_sdk       = { version = "0.27", features = ["rt-tokio"], optional = true }
opentelemetry-otlp      = { version = "0.27", features = ["grpc-tonic"], optional = true }
opentelemetry-appender-tracing = { version = "0.28", optional = true }
tracing-opentelemetry   = { version = "0.28", optional = true }
```

Note: `tracing` and `tracing-subscriber` are already in `Cargo.toml`. This phase also adds the missing `tracing_subscriber::fmt().with_env_filter().init()` call that is currently absent from the codebase.

---

## Tests

All in `tests/test_telemetry.rs`, gated `#[cfg(feature = "otel")]`:

### 9.1 Sampler unit tests
Instantiate `SmartSampler` directly (no OTel infrastructure). Assert:
- `force_sample = true` always sampled
- `sample_ratio = 0.0` always drops (except force_sample)
- `sample_ratio = 1.0` always records
- Parent-sampled context propagates RECORD_AND_SAMPLE regardless of ratio

### 9.2 Metrics recording
Use `opentelemetry_sdk::testing::metrics::InMemoryMetricReader` (no collector needed). Assert:
- Mock completions request → `modelrouter.requests.total` increments
- Policy-denied request → `modelrouter.policy.denied` increments with correct `reason` label

### 9.3 Init/shutdown
Assert `init_telemetry()` succeeds with a valid `TelemetryConfig`, and that `TelemetryShutdownGuard::drop()` completes without panicking. Verifies the startup path doesn't crash on a valid config.

### 9.4 Span attribute coverage
Use `opentelemetry_sdk::testing::trace::InMemorySpanExporter`. Fire a mock completions request via `axum-test`, building the test `AppState` using the `in_memory_db()` helper already in `tests/common/mod.rs`. Assert the resulting span has `model`, `provider`, `cost.usd`, and `tokens.prompt` attributes set.

---

## File Changes Summary

| File | Change |
|------|--------|
| `Cargo.toml` | Add 5 optional OTel dependencies + `otel` feature |
| `src/config/schema.rs` | Add `TelemetryConfig` struct (cfg-gated) |
| `src/config/mod.rs` | Load `[telemetry]` section when feature enabled |
| `src/telemetry/mod.rs` | New — `init_telemetry()`, `TelemetryShutdownGuard` |
| `src/telemetry/sampler.rs` | New — `SmartSampler` |
| `src/telemetry/metrics.rs` | New — all instrument statics |
| `src/lib.rs` | Add `pub mod telemetry` (cfg-gated) |
| `src/cli/mod.rs` | Add subscriber init; call `init_telemetry` before serve |
| `src/api/app.rs` | Wire `tower-http` `TraceLayer` |
| `src/api/routes/completions.rs` | `#[instrument]`, span attributes, metrics recording |
| `src/hooks/pipeline.rs` | `#[instrument]`, hook span attributes |
| `src/hooks/lifecycle.rs` | `#[instrument]`, hook span attributes |
| `src/router/policy.rs` | `#[instrument]` on `check()` |
| `config.example.toml` | Add `[telemetry]` example section |
| `tests/test_telemetry.rs` | New — 4 test groups (cfg-gated) |

---

## Success Criteria

- [ ] `cargo build` (no feature) produces same binary as before — zero OTel overhead
- [ ] `cargo build --features otel` compiles cleanly
- [ ] `cargo test --features otel` passes with 0 failures
- [ ] With `telemetry.enabled = true` and a running collector, spans appear in the backend
- [ ] With `telemetry.enabled = false`, no connection attempts to the OTel endpoint
- [ ] `TelemetryShutdownGuard::drop()` flushes without deadlock on clean shutdown
- [ ] `sample_ratio = 0.0` produces no traces except errors and force-sampled spans
