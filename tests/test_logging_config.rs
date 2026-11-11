//! `[logging]`: the file sink settings parse, default, and are validated when
//! the config loads, before anything starts.

use modelrouter::config::schema::{LogFormat, LogRotation, LoggingConfig};

fn load(toml: &str) -> anyhow::Result<modelrouter::config::Settings> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, toml).unwrap();
    modelrouter::config::load_from_path(path.to_str().unwrap())
}

#[test]
fn an_absent_logging_section_is_stdout_only_with_defaults() {
    let s = load("").unwrap();
    let l = &s.logging;
    assert_eq!(l.format, LogFormat::Text);
    assert!(l.file.is_none());
    assert!(l.stdout_enabled());
    assert_eq!(l.rotate, LogRotation::Daily);
    assert_eq!(l.max_size_mb, 50);
    assert_eq!(l.max_files, 7);
    assert_eq!(l.min_free_mb, 512);
}

#[test]
fn a_full_logging_section_parses() {
    let s = load(
        r#"
[logging]
file = "/var/log/modelrouter/modelrouter.log"
rotate = "size"
max_size_mb = 20
max_files = 3
format = "json"
stdout = true
min_free_mb = 0
"#,
    )
    .unwrap();
    let l = &s.logging;
    assert_eq!(l.file.as_deref(), Some(std::path::Path::new("/var/log/modelrouter/modelrouter.log")));
    assert_eq!(l.rotate, LogRotation::Size);
    assert_eq!((l.max_size_mb, l.max_files, l.min_free_mb), (20, 3, 0));
    assert_eq!(l.format, LogFormat::Json);
    assert!(l.stdout_enabled());
}

#[test]
fn hourly_parses_and_an_unknown_rotation_is_refused() {
    assert_eq!(load("[logging]\nrotate = \"hourly\"\n").unwrap().logging.rotate, LogRotation::Hourly);
    assert!(load("[logging]\nrotate = \"weekly\"\n").is_err());
}

#[test]
fn zero_max_files_is_refused_at_load() {
    let err = load("[logging]\nfile = \"/tmp/mr-test.log\"\nmax_files = 0\n").unwrap_err();
    assert!(err.to_string().contains("logging.max_files = 0"), "{err}");
}

#[test]
fn zero_max_size_is_refused_at_load() {
    let err = load("[logging]\nfile = \"/tmp/mr-test.log\"\nmax_size_mb = 0\n").unwrap_err();
    assert!(err.to_string().contains("logging.max_size_mb = 0"), "{err}");
}

#[test]
fn a_directory_as_the_log_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = LoggingConfig { file: Some(dir.path().to_path_buf()), ..Default::default() };
    let err = cfg.validate().unwrap_err();
    assert!(err.to_string().contains("is a directory"), "{err}");
}

#[test]
fn a_path_with_no_file_name_is_refused() {
    for p in ["", "/", "logs/.."] {
        let cfg = LoggingConfig { file: Some(p.into()), ..Default::default() };
        assert!(cfg.validate().is_err(), "{p:?} should be refused");
    }
}

#[test]
fn caps_are_not_checked_when_there_is_no_file() {
    let cfg = LoggingConfig { max_files: 0, max_size_mb: 0, ..Default::default() };
    cfg.validate().unwrap();
}

#[test]
fn stdout_off_without_a_file_is_refused_because_nothing_would_be_logged() {
    let err = load("[logging]\nstdout = false\n").unwrap_err();
    assert!(err.to_string().contains("discard every log line"), "{err}");
    load("[logging]\nstdout = false\nfile = \"/tmp/mr-test.log\"\n").unwrap();
}
