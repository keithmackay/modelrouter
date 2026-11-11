//! The log output: stdout, a rotating file (`[logging] file`), or both, in
//! the format `[logging] format` selects.

pub mod rotating;

use std::path::Path;

use tracing::Subscriber;
use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::config::schema::{LogFormat, LoggingConfig};

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

/// The fmt layer for a file: as [`fmt_layer`], without terminal colour codes.
fn file_fmt_layer<S, W>(format: LogFormat, writer: W) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    match format {
        LogFormat::Text => tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(writer)
            .boxed(),
        LogFormat::Json => fmt_layer(format, writer),
    }
}

/// Keeps the file sink's writer thread alive. Dropping it flushes the lines
/// still queued, so hold it until the process exits.
#[must_use = "dropping the guard stops the log file writer"]
#[derive(Default)]
pub struct LogGuard {
    _worker: Option<WorkerGuard>,
}

/// The output layers `config` asks for: stdout, the rotating file, or both.
/// Fails when the log file cannot be opened, naming the path.
pub fn output_layer<S>(
    config: &LoggingConfig,
) -> anyhow::Result<(Box<dyn Layer<S> + Send + Sync>, LogGuard)>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    output_layer_with_stdout(config, std::io::stdout)
}

/// [`output_layer`] with the stdout writer supplied, so tests can capture it.
fn output_layer_with_stdout<S, W>(
    config: &LoggingConfig,
    stdout_writer: W,
) -> anyhow::Result<(Box<dyn Layer<S> + Send + Sync>, LogGuard)>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let stdout = config
        .stdout_enabled()
        .then(|| fmt_layer::<S, _>(config.format, stdout_writer));
    let (file, guard) = match &config.file {
        None => (None, LogGuard::default()),
        Some(path) => {
            let writer = rotating::RotatingFile::open(
                path,
                config.rotate,
                config.max_size_mb.saturating_mul(1024 * 1024),
                config.max_files,
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "logging.file = {}: cannot open it for writing ({e}). \
                     Check the directory exists or can be created and is writable \
                     by this user. Refusing to start.",
                    path.display()
                )
            })?;
            // Blocking when the queue is full, never dropping a line.
            let (non_blocking, worker) = NonBlockingBuilder::default()
                .lossy(false)
                .thread_name("modelrouter-log-file")
                .finish(writer);
            (
                Some(file_fmt_layer::<S, _>(config.format, non_blocking)),
                LogGuard { _worker: Some(worker) },
            )
        }
    };
    // Never combine with an absent `Option` layer: boxed, it reports a level
    // hint of OFF that tracing-subscriber cannot tell from a real one, and the
    // combined layer would then silence the layer that is present.
    let layer = match (stdout, file) {
        (Some(out), Some(file)) => out.and_then(file).boxed(),
        (Some(only), None) | (None, Some(only)) => only,
        (None, None) => tracing_subscriber::layer::Identity::new().boxed(),
    };
    Ok((layer, guard))
}

/// Install the global subscriber: env filter plus the configured outputs.
/// A subscriber already installed (as in tests) is left in place.
pub fn init(config: &LoggingConfig) -> anyhow::Result<LogGuard> {
    let (output, guard) = output_layer(config)?;
    tracing_subscriber::registry()
        .with(env_filter())
        .with(output)
        .try_init()
        .ok();
    warn_on_low_free_space(config);
    Ok(guard)
}

/// Free bytes available to this user on the filesystem holding `dir`.
#[cfg(unix)]
pub fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a writable statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

#[cfg(not(unix))]
pub fn free_bytes(_dir: &Path) -> Option<u64> {
    None
}

/// The warning to log when the log volume is below the floor, if it is.
fn low_free_space_message(config: &LoggingConfig, free: Option<u64>) -> Option<String> {
    let path = config.file.as_ref()?;
    if config.min_free_mb == 0 {
        return None;
    }
    let floor = config.min_free_mb.saturating_mul(1024 * 1024);
    let free = free?;
    (free < floor).then(|| {
        format!(
            "log volume for {} has {free} bytes free, below the logging.min_free_mb floor \
             of {floor} bytes ({} MiB); the logs can use up to {} MiB",
            path.display(),
            config.min_free_mb,
            (config.max_files as u64 + 1).saturating_mul(config.max_size_mb),
        )
    })
}

/// Warn, not fail, when the log file's volume has less free space than
/// `logging.min_free_mb`.
pub fn warn_on_low_free_space(config: &LoggingConfig) {
    let Some(path) = &config.file else { return };
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if let Some(msg) = low_free_space_message(config, free_bytes(dir)) {
        tracing::warn!(path = %path.display(), "{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::Registry;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }

    fn file_config(path: &Path) -> LoggingConfig {
        LoggingConfig { file: Some(path.to_path_buf()), ..LoggingConfig::default() }
    }

    #[test]
    fn stdout_is_on_without_a_file_and_off_with_one_unless_asked() {
        let mut c = LoggingConfig::default();
        assert!(c.stdout_enabled());
        c.file = Some("/tmp/x.log".into());
        assert!(!c.stdout_enabled());
        c.stdout = Some(true);
        assert!(c.stdout_enabled());
        c.file = None;
        c.stdout = Some(false);
        assert!(!c.stdout_enabled());
    }

    #[test]
    fn the_file_sink_writes_plain_lines_and_flushes_on_guard_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/app.log");
        let (layer, guard) = output_layer::<Registry>(&file_config(&path)).unwrap();
        let subscriber = Registry::default().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(answer = 42, "hello file");
        });
        drop(guard);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("hello file"), "{text}");
        assert!(text.contains("answer=42"), "{text}");
        assert!(!text.contains('\u{1b}'), "no ANSI codes in a file: {text:?}");
    }

    #[test]
    fn the_file_sink_writes_json_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let config = LoggingConfig { format: LogFormat::Json, ..file_config(&path) };
        let (layer, guard) = output_layer::<Registry>(&config).unwrap();
        tracing::subscriber::with_default(Registry::default().with(layer), || {
            tracing::info!(answer = 42, "hello json");
        });
        drop(guard);
        let line = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["message"], "hello json");
        assert_eq!(v["answer"], 42);
    }

    #[test]
    fn an_unwritable_log_path_fails_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let path = blocker.join("app.log");
        let err = output_layer::<Registry>(&file_config(&path)).err().unwrap();
        let msg = err.to_string();
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        assert!(msg.contains("Refusing to start"), "{msg}");
    }

    /// With no `file`, the output is the stdout layer that existed before the
    /// file sink, so the bytes written are identical apart from the timestamp.
    #[test]
    fn without_a_file_the_output_matches_the_plain_stdout_layer() {
        let render = |layer: Box<dyn Layer<Registry> + Send + Sync>| {
            tracing::subscriber::with_default(Registry::default().with(layer), || {
                tracing::info!(k = "v", "same bytes");
            });
        };
        let strip = |v: &[u8]| -> String {
            let s = String::from_utf8(v.to_vec()).unwrap();
            match serde_json::from_str::<serde_json::Value>(s.trim()) {
                Ok(mut j) => {
                    j.as_object_mut().unwrap().remove("timestamp");
                    j.to_string()
                }
                // Text: drop the leading timestamp, keep the rest verbatim.
                Err(_) => s.split_once(' ').unwrap_or_else(|| panic!("{s:?}")).1.to_string(),
            }
        };
        for format in [LogFormat::Text, LogFormat::Json] {
            let config = LoggingConfig { format, ..LoggingConfig::default() };
            let (before, after) = (Buf::default(), Buf::default());
            render(fmt_layer(format, before.clone()));
            let (layer, guard) = output_layer_with_stdout::<Registry, _>(&config, after.clone()).unwrap();
            assert!(guard._worker.is_none(), "no file writer without a file");
            render(layer);
            let (b, a) = (before.0.lock().unwrap().clone(), after.0.lock().unwrap().clone());
            assert!(!b.is_empty() && !a.is_empty(), "{format:?} before={b:?} after={a:?}");
            assert_eq!(strip(&b), strip(&a), "{format:?}");
        }
    }

    #[test]
    fn stdout_off_and_no_file_writes_nothing() {
        let config = LoggingConfig { stdout: Some(false), ..LoggingConfig::default() };
        let out = Buf::default();
        let (layer, _guard) = output_layer_with_stdout::<Registry, _>(&config, out.clone()).unwrap();
        tracing::subscriber::with_default(Registry::default().with(layer), || {
            tracing::info!("nowhere");
        });
        assert!(out.0.lock().unwrap().is_empty());
    }

    #[test]
    fn stdout_and_file_together_both_receive_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let config = LoggingConfig { stdout: Some(true), ..file_config(&path) };
        let out = Buf::default();
        let (layer, guard) = output_layer_with_stdout::<Registry, _>(&config, out.clone()).unwrap();
        tracing::subscriber::with_default(Registry::default().with(layer), || {
            tracing::info!("to both");
        });
        drop(guard);
        assert!(String::from_utf8(out.0.lock().unwrap().clone()).unwrap().contains("to both"));
        assert!(std::fs::read_to_string(&path).unwrap().contains("to both"));
    }

    #[test]
    fn low_free_space_warns_with_path_free_bytes_and_floor() {
        let config = LoggingConfig { min_free_mb: 100, ..file_config(Path::new("/var/log/mr/app.log")) };
        let msg = low_free_space_message(&config, Some(1024)).unwrap();
        assert!(msg.contains("/var/log/mr/app.log"), "{msg}");
        assert!(msg.contains("1024 bytes free"), "{msg}");
        assert!(msg.contains(&(100u64 * 1024 * 1024).to_string()), "{msg}");
        assert!(low_free_space_message(&config, Some(200 * 1024 * 1024)).is_none());
        assert!(low_free_space_message(&config, None).is_none());
        let off = LoggingConfig { min_free_mb: 0, ..config.clone() };
        assert!(low_free_space_message(&off, Some(0)).is_none());
        let no_file = LoggingConfig { min_free_mb: 100, ..LoggingConfig::default() };
        assert!(low_free_space_message(&no_file, Some(0)).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn free_bytes_reads_the_volume() {
        assert!(free_bytes(Path::new("/")).is_some());
        assert!(free_bytes(Path::new("/definitely/not/a/path/here")).is_none());
    }

    #[test]
    fn warn_on_low_free_space_runs_for_relative_and_absolute_paths() {
        // No assertion on output: this exercises the directory resolution.
        warn_on_low_free_space(&LoggingConfig::default());
        warn_on_low_free_space(&LoggingConfig { min_free_mb: u64::MAX / (1 << 20), ..file_config(Path::new("app.log")) });
        warn_on_low_free_space(&file_config(Path::new("/tmp/app.log")));
    }

    #[test]
    fn init_with_a_file_returns_a_live_guard() {
        let dir = tempfile::tempdir().unwrap();
        let guard = init(&file_config(&dir.path().join("app.log"))).unwrap();
        assert!(guard._worker.is_some());
    }
}
