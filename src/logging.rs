//! The stdout log layer, in the format `[logging] format` selects.

use tracing::Subscriber;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::config::schema::LogFormat;

/// `RUST_LOG` when set, else `info`.
pub fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// A fmt layer writing to `writer`. `Json` writes one object per event with
/// the event's fields flattened beside `timestamp`, `level` and `target`.
pub fn fmt_layer<S, W>(format: LogFormat, writer: W) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    match format {
        LogFormat::Text => tracing_subscriber::fmt::layer().with_writer(writer).boxed(),
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_writer(writer)
            .boxed(),
    }
}

/// Install the global subscriber: env filter plus the stdout layer.
/// A subscriber already installed (as in tests) is left in place.
pub fn init(format: LogFormat) {
    tracing_subscriber::registry()
        .with(env_filter())
        .with(fmt_layer(format, std::io::stdout))
        .try_init()
        .ok();
}
