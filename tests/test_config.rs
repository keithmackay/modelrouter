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
