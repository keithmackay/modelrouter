use serial_test::serial;

#[test]
#[serial]
fn default_settings_parse_without_config_file() {
    let s = modelrouter::config::load(Some("/nonexistent/path.toml".into()))
        .expect("should fall back to defaults");
    assert_eq!(s.server.port, 8080);
    assert_eq!(s.routing.default_model, "gpt-4o");
}

#[test]
#[serial]
fn env_var_overrides_config() {
    std::env::set_var("MODELROUTER_SERVER__PORT", "9090");
    let s = modelrouter::config::load(Some("/nonexistent/path.toml".into())).unwrap();
    assert_eq!(s.server.port, 9090);
    std::env::remove_var("MODELROUTER_SERVER__PORT");
}

#[cfg(feature = "otel")]
#[test]
fn telemetry_config_has_defaults() {
    let s = modelrouter::config::schema::TelemetryConfig::default();
    assert_eq!(s.enabled, false);
    assert_eq!(s.endpoint, "http://localhost:4317");
    assert_eq!(s.service_name, "modelrouter");
    assert!((s.sample_ratio - 0.1).abs() < f64::EPSILON);
    assert_eq!(s.slow_threshold_ms, 2000);
}

#[test]
#[serial]
fn cache_config_parses_nested_class_policies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
[cache]
enabled = true
backend = "redis"
redis_url = "redis://localhost:6379"
namespace = "staging"
max_entries = 50
ttl_seconds = 120

[cache.completions]
max_temperature = 0.3
ttl_seconds = 600

[cache.search]
ttl_seconds = 30
"#,
    )
    .unwrap();

    let s = modelrouter::config::load(Some(path)).unwrap();
    assert!(s.cache.enabled);
    assert_eq!(s.cache.backend, "redis");
    assert_eq!(s.cache.redis_url, "redis://localhost:6379");
    assert_eq!(s.cache.namespace, "staging");
    assert_eq!(s.cache.max_entries, 50);
    assert_eq!(s.cache.ttl_seconds, 120);
    assert_eq!(s.cache.completions.max_temperature, 0.3);
    assert_eq!(s.cache.completions.ttl_seconds, Some(600));
    // Unset fields keep their conservative defaults.
    assert!(s.cache.completions.enabled);
    assert_eq!(s.cache.completions.assumed_temperature, 1.0);
    assert_eq!(s.cache.search.ttl_seconds, 30);
}

#[test]
#[serial]
fn cache_is_disabled_by_default() {
    let s = modelrouter::config::load(Some("/nonexistent/path.toml".into())).unwrap();
    assert!(!s.cache.enabled);
    assert_eq!(s.cache.backend, "memory");
    assert_eq!(s.cache.completions.max_temperature, 0.0);
}
