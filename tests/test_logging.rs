use std::io::Write;
use std::sync::{Arc, Mutex};

use modelrouter::config::schema::LogFormat;
use serial_test::serial;
use tracing_subscriber::layer::SubscriberExt;

/// A `MakeWriter` that collects everything written into one shared buffer.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Emit a representative mix of events (fields, a span, a multi-line message
/// with quotes) through the layer for `format`, and return the output lines.
fn emit(format: LogFormat) -> Vec<String> {
    let out = Captured::default();
    let subscriber = tracing_subscriber::registry()
        .with(modelrouter::logging::fmt_layer(format, out.clone()));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(tier = "fast", provider = "openai", status = 200u16, latency_ms = 42u64, "request complete");
        let span = tracing::info_span!("request", request_id = "abc");
        let _entered = span.enter();
        tracing::warn!("upstream said \"slow down\"\nretrying");
        tracing::error!(error = %"boom", "provider failed");
    });
    let bytes = out.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap().lines().map(str::to_string).collect()
}

#[test]
fn json_format_writes_one_object_per_line_with_flattened_fields() {
    let lines = emit(LogFormat::Json);
    assert_eq!(lines.len(), 3, "one line per event: {lines:#?}");
    let objects: Vec<serde_json::Value> = lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect();
    assert!(objects.iter().all(|o| o.is_object()));

    let first = &objects[0];
    assert_eq!(first["level"], "INFO");
    assert_eq!(first["message"], "request complete");
    assert_eq!(first["tier"], "fast");
    assert_eq!(first["provider"], "openai");
    assert_eq!(first["status"], 200);
    assert_eq!(first["latency_ms"], 42);
    assert!(first["timestamp"].is_string());

    assert_eq!(objects[1]["message"], "upstream said \"slow down\"\nretrying");
    assert_eq!(objects[1]["span"]["request_id"], "abc");
    assert_eq!(objects[2]["error"], "boom");
}

#[test]
fn text_format_is_the_default_and_stays_human_readable() {
    assert_eq!(LogFormat::default(), LogFormat::Text);
    let lines = emit(LogFormat::Text);
    assert!(lines.iter().any(|l| l.contains("request complete") && l.contains("tier") && l.contains("fast")));
    assert!(lines.iter().all(|l| serde_json::from_str::<serde_json::Value>(l).is_err()));
}

fn load_toml(body: &str) -> anyhow::Result<modelrouter::config::Settings> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, body).unwrap();
    modelrouter::config::load(Some(path))
}

#[test]
#[serial]
fn logging_format_parses_from_config_and_env() {
    assert_eq!(load_toml("").unwrap().logging.format, LogFormat::Text);
    assert_eq!(load_toml("[logging]\nformat = \"json\"\n").unwrap().logging.format, LogFormat::Json);

    std::env::set_var("MODELROUTER_LOGGING__FORMAT", "json");
    let from_env = load_toml("");
    std::env::remove_var("MODELROUTER_LOGGING__FORMAT");
    assert_eq!(from_env.unwrap().logging.format, LogFormat::Json);
}

#[test]
#[serial]
fn unknown_logging_format_fails_to_load() {
    let err = load_toml("[logging]\nformat = \"xml\"\n").unwrap_err();
    assert!(err.to_string().contains("xml"), "{err}");
}
