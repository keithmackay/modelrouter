#![cfg(feature = "otel")]

pub mod metrics;
pub mod sampler;

use std::time::Duration;
use anyhow::Result;
use opentelemetry::KeyValue;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    metrics::{PeriodicReader, SdkMeterProvider},
    runtime,
    trace::{BatchConfigBuilder, BatchSpanProcessor, TracerProvider},
    Resource,
};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::schema::TelemetryConfig;
use sampler::SmartSampler;

/// Holds pipeline handles. All pipelines are flushed and shut down on Drop.
pub struct TelemetryShutdownGuard {
    meter_provider: SdkMeterProvider,
    log_provider: opentelemetry_sdk::logs::LoggerProvider,
}

impl Drop for TelemetryShutdownGuard {
    fn drop(&mut self) {
        opentelemetry::global::shutdown_tracer_provider();
        if let Err(e) = self.log_provider.shutdown() {
            tracing::warn!("OTel log provider shutdown error: {e}");
        }
        if let Err(e) = self.meter_provider.force_flush() {
            tracing::warn!("OTel meter flush error: {e}");
        }
        if let Err(e) = self.meter_provider.shutdown() {
            tracing::warn!("OTel meter shutdown error: {e}");
        }
    }
}

/// Build all three OTel pipelines and install the layered tracing subscriber.
/// Returns a guard; drop it to flush on shutdown.
pub fn init_telemetry(config: &TelemetryConfig) -> Result<TelemetryShutdownGuard> {
    let resource = Resource::new(vec![
        KeyValue::new("service.name", config.service_name.clone()),
    ]);

    // ── Traces ──────────────────────────────────────────────────────────
    let trace_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&config.endpoint)
        .build()?;

    let batch_config = BatchConfigBuilder::default()
        .with_max_queue_size(config.batch_queue_size)
        .with_scheduled_delay(Duration::from_millis(config.batch_scheduled_delay_ms))
        .with_max_export_batch_size(config.batch_max_export_size)
        .build();

    let trace_provider = TracerProvider::builder()
        .with_resource(resource.clone())
        .with_sampler(SmartSampler::new(config.sample_ratio))
        .with_span_processor(
            BatchSpanProcessor::builder(trace_exporter, runtime::Tokio)
                .with_batch_config(batch_config)
                .build(),
        )
        .build();
    opentelemetry::global::set_tracer_provider(trace_provider.clone());

    // ── Metrics ─────────────────────────────────────────────────────────
    let metrics_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(&config.endpoint)
        .build()?;

    let reader = PeriodicReader::builder(metrics_exporter, runtime::Tokio)
        .with_interval(Duration::from_secs(15))
        .build();

    let meter_provider = SdkMeterProvider::builder()
        .with_resource(resource.clone())
        .with_reader(reader)
        .build();
    opentelemetry::global::set_meter_provider(meter_provider.clone());

    // Initialise metric instruments
    // Leak the service name once to obtain a `&'static str` for the meter name.
    let meter_name: &'static str = Box::leak(config.service_name.clone().into_boxed_str());
    metrics::init_instruments(meter_provider.meter(meter_name));

    // ── Logs ────────────────────────────────────────────────────────────
    let log_exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .with_endpoint(&config.endpoint)
        .build()
        .map_err(|e| anyhow::anyhow!("log exporter build error: {e}"))?;

    let log_provider = opentelemetry_sdk::logs::LoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(log_exporter, runtime::Tokio)
        .build();
    let log_bridge = OpenTelemetryTracingBridge::new(&log_provider);

    // ── Subscriber ──────────────────────────────────────────────────────
    let tracer = trace_provider.tracer(config.service_name.clone());
    let otel_trace_layer = OpenTelemetryLayer::new(tracer);

    // try_init() instead of init() so tests don't panic on duplicate subscriber
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .with(otel_trace_layer)
        .with(log_bridge)
        .try_init()
        .ok(); // ok to fail if subscriber already set (e.g. in tests)

    Ok(TelemetryShutdownGuard { meter_provider, log_provider })
}
